//! Per-client connection lifecycle — reader, writer, and registration.
//!
//! Each connection is managed by a [`Connection`]. It reads IRC lines from
//! a transport-agnostic reader, handles the IRC registration handshake
//! (NICK + USER → welcome burst), and then dispatches commands to the handler.
//!
//! The writer side is always an `mpsc::Sender<Arc<str>>` — the actual transport
//! (TCP socket, WebSocket, TLS, etc.) drains that channel in its own write loop.

use std::sync::Arc;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};

use crate::client::{ClientHandle, ClientId, cap};
use crate::handler;
use crate::relay::RelayEvent;
use crate::sasl::{self, SaslError, SaslStep};
use crate::state::SharedState;

/// Size of the per-client outgoing message buffer.
///
/// Each slot holds an `Arc<str>` (pointer + refcount = 16 bytes on 64-bit).
/// 4096 slots = 64 KB of pointers per client at maximum fill.  This is large
/// enough to absorb a full channel join burst (NAMES/WHO reply) without
/// disconnecting the client as a false slow-client positive.
const SEND_BUFFER: usize = 4096;

/// Maximum time (seconds) a client may spend in the registration phase
/// (before sending NICK + USER) before the connection is dropped.
const REGISTRATION_TIMEOUT_SECS: u64 = 30;

/// Manages a single client connection from accept to disconnect.
pub struct Connection {
    id: ClientId,
    state: SharedState,
    hostname: String,
}

impl Connection {
    pub fn new(id: ClientId, state: SharedState, hostname: String) -> Self {
        Self {
            id,
            state,
            hostname,
        }
    }

    /// Run the connection over a plain TCP stream.
    pub async fn run_tcp(self, stream: TcpStream) {
        let (reader, writer) = stream.into_split();
        let (tx, rx) = mpsc::channel::<Arc<str>>(SEND_BUFFER);
        let cancel = CancellationToken::new();

        // Spawn the writer task for TCP.
        let writer_handle = tokio::spawn(write_loop(writer, rx, cancel.clone()));

        // Run the reader (registration + command dispatch).
        self.read_loop(BufReader::new(reader), tx, cancel).await;

        // Reader is done — the writer will finish once tx is dropped.
        let _ = writer_handle.await;

        info!(client_id = %self.id, "connection closed");
    }

    /// Run the connection over a TLS-wrapped TCP stream.
    pub async fn run_tls(self, stream: TlsStream<TcpStream>) {
        let (reader, writer) = tokio::io::split(stream);
        let (tx, rx) = mpsc::channel::<Arc<str>>(SEND_BUFFER);
        let cancel = CancellationToken::new();

        // Spawn the writer task for TLS.
        let writer_handle = tokio::spawn(write_loop(writer, rx, cancel.clone()));

        // Run the reader (registration + command dispatch).
        self.read_loop(BufReader::new(reader), tx, cancel).await;

        // Reader is done — the writer will finish once tx is dropped.
        let _ = writer_handle.await;

        info!(client_id = %self.id, "TLS connection closed");
    }

    /// Run the connection over a generic line-based reader.
    ///
    /// The caller is responsible for:
    /// - Providing a buffered reader that yields `\n`-terminated IRC lines.
    /// - Spawning a writer task that drains `rx` and sends lines to the client
    ///   (e.g. as WebSocket text frames).
    /// - Creating a `CancellationToken` and passing clones to both the writer
    ///   task and here; `write_loop` will cancel the token on slow-client disconnect.
    ///
    /// Returns when the reader hits EOF or the client sends QUIT.
    pub async fn run_generic<R: AsyncBufRead + Unpin + Send + 'static>(
        self,
        reader: R,
        tx: mpsc::Sender<Arc<str>>,
        cancel: CancellationToken,
    ) {
        self.read_loop(reader, tx, cancel).await;
        info!(client_id = %self.id, "connection closed");
    }

    /// Read lines from a buffered reader, handle registration, then dispatch.
    async fn read_loop<R: AsyncBufRead + Unpin>(
        &self,
        mut reader: R,
        tx: mpsc::Sender<Arc<str>>,
        cancel: CancellationToken,
    ) {
        // --- Registration phase ---
        let reg_timeout = Duration::from_secs(REGISTRATION_TIMEOUT_SECS);
        let client = match tokio::time::timeout(
            reg_timeout,
            self.registration_phase(&mut reader, &tx, cancel),
        )
        .await
        {
            Ok(Some(c)) => c,
            Ok(None) => return, // Connection closed or failed during registration.
            Err(_elapsed) => {
                warn!(client_id = %self.id, "registration timeout — dropping connection");
                return;
            }
        };

        info!(
            client_id = %self.id,
            nick = %client.info.nick,
            "client registered"
        );

        // --- Command dispatch phase ---
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let line = line_buf.trim_end();
                    if line.is_empty() {
                        continue;
                    }
                    match IrcMessage::parse(line) {
                        Ok(msg) => {
                            // QUIT is special — handler will clean up and we should exit.
                            let is_quit = msg.command == Command::Quit;
                            handler::handle_command(&self.state, self.id, &msg).await;
                            if is_quit {
                                return;
                            }
                        }
                        Err(e) => {
                            debug!(client_id = %self.id, error = %e, "parse error");
                        }
                    }
                }
                Err(e) => {
                    debug!(client_id = %self.id, error = %e, "read error");
                    break;
                }
            }
        }

        // Client disconnected without QUIT.
        handle_unexpected_disconnect(&self.state, self.id).await;
    }

    /// Handle the registration handshake: wait for NICK + USER (+ optional CAP
    /// negotiation and SASL), validate, register, send welcome burst.
    ///
    /// CAP flow:
    /// - `CAP LS [302]` → reply `CAP * LS :sasl`
    /// - `CAP LIST`     → reply `CAP * LIST :`
    /// - `CAP REQ :sasl`→ reply `CAP * ACK :sasl`
    /// - `CAP REQ ...`  → reply `CAP * NAK :...` (reject unknown caps)
    /// - `CAP END`      → clear `cap_negotiating`; complete registration if
    ///   NICK+USER already received
    ///
    /// SASL flow:
    /// - `AUTHENTICATE <MECH>` → start SASL session; send `AUTHENTICATE +`
    /// - `AUTHENTICATE <data>` → step mechanism; send challenge or finish
    /// - On success: send 900 RPL_LOGGEDIN, 903 RPL_SASLSUCCESS
    /// - On failure: send 904 ERR_SASLFAIL
    ///
    /// Registration is deferred until both CAP negotiation is finished AND
    /// NICK+USER have been received.
    async fn registration_phase<R: AsyncBufRead + Unpin>(
        &self,
        reader: &mut R,
        tx: &mpsc::Sender<Arc<str>>,
        cancel: CancellationToken,
    ) -> Option<ClientHandle> {
        let mut pending_nick: Option<String> = None;
        let mut pending_user: Option<(String, String)> = None; // (username, realname)
        let mut cap_negotiating = false;
        let mut pending_caps: u32 = 0;
        let mut sasl_session: Option<sasl::SaslSession> = None;
        let mut authenticated_account: Option<String> = None;
        let mut line_buf = String::new();

        // Helper to send a raw line during pre-registration (no ClientHandle yet).
        let send_raw = |line: String| {
            let tx = tx.clone();
            async move {
                let arc: Arc<str> = line.into();
                let _ = tx.send(arc).await;
            }
        };

        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => return None,
                Ok(_) => {}
                Err(e) => {
                    debug!(client_id = %self.id, error = %e, "read error during registration");
                    return None;
                }
            }

            let line = line_buf.trim_end();
            if line.is_empty() {
                continue;
            }

            let msg = match IrcMessage::parse(line) {
                Ok(m) => m,
                Err(_) => continue,
            };

            match &msg.command {
                Command::Cap => {
                    let subcommand = msg.params.first().map(|s| s.to_ascii_uppercase());
                    match subcommand.as_deref() {
                        Some("LS") => {
                            // Begin CAP negotiation; advertise SASL and all supported caps.
                            cap_negotiating = true;
                            let nick = pending_nick.as_deref().unwrap_or("*");
                            let other_caps = cap::supported_caps().join(" ");
                            let reply = format!(
                                ":{} CAP {} LS :sasl={} {}",
                                self.state.server_name(),
                                nick,
                                crate::sasl::SUPPORTED_MECHANISMS,
                                other_caps,
                            );
                            send_raw(reply).await;
                        }
                        Some("LIST") => {
                            let nick = pending_nick.as_deref().unwrap_or("*");
                            // Build the active caps list from the pending bitmask.
                            let active: Vec<&str> = cap::supported_caps()
                                .iter()
                                .copied()
                                .filter(|&name| {
                                    cap::name_to_bit(name)
                                        .is_some_and(|bit| pending_caps & bit != 0)
                                })
                                .collect();
                            let list_str = active.join(" ");
                            let reply = format!(
                                ":{} CAP {} LIST :{}",
                                self.state.server_name(),
                                nick,
                                list_str,
                            );
                            send_raw(reply).await;
                        }
                        Some("REQ") => {
                            let nick = pending_nick.as_deref().unwrap_or("*");
                            let caps_raw = msg.params.get(1).map(|s| s.as_str()).unwrap_or("");
                            // Strip leading ':' if present (some clients include it).
                            let caps = caps_raw.trim_start_matches(':');

                            // Split the requested caps; each must be supported (or prefixed
                            // with `-` for disable). We accept "sasl" and any cap in
                            // cap::supported_caps().
                            let all_ok = !caps.is_empty()
                                && caps.split_whitespace().all(|c| {
                                    let name = c.strip_prefix('-').unwrap_or(c);
                                    name.eq_ignore_ascii_case("sasl")
                                        || cap::name_to_bit(&name.to_ascii_lowercase()).is_some()
                                });

                            if all_ok {
                                // Apply enable/disable to pending_caps.
                                for token in caps.split_whitespace() {
                                    if let Some(name) = token.strip_prefix('-') {
                                        if let Some(bit) =
                                            cap::name_to_bit(&name.to_ascii_lowercase())
                                        {
                                            pending_caps &= !bit;
                                        }
                                    } else {
                                        if let Some(bit) =
                                            cap::name_to_bit(&token.to_ascii_lowercase())
                                        {
                                            pending_caps |= bit;
                                        }
                                        // "sasl" has no bit in cap module — just ACK it.
                                    }
                                }
                                let reply = format!(
                                    ":{} CAP {} ACK :{}",
                                    self.state.server_name(),
                                    nick,
                                    caps
                                );
                                send_raw(reply).await;
                            } else {
                                let reply = format!(
                                    ":{} CAP {} NAK :{}",
                                    self.state.server_name(),
                                    nick,
                                    caps_raw
                                );
                                send_raw(reply).await;
                            }
                        }
                        Some("END") => {
                            cap_negotiating = false;
                            // Fall through to the registration-completion check below.
                        }
                        _ => {
                            // Unknown CAP subcommand — ignore.
                        }
                    }
                }

                Command::Authenticate => {
                    let param = msg.params.first().map(|s| s.as_str()).unwrap_or("+");

                    // If no session yet, param is the mechanism name.
                    if sasl_session.is_none() {
                        let mechanism_name = param.to_ascii_uppercase();

                        // Build a sync PasswordLookup that does a single-entry lookup.
                        // The std RwLock read is an in-memory operation that does not
                        // need block_in_place; the CPU-heavy PBKDF2 work is offloaded
                        // via spawn_blocking further down.
                        let ns_opt = self.state.services().map(|s| s.nickserv.clone());
                        let lookup: sasl::PasswordLookup = Box::new(move |nick: &str| {
                            let ns = ns_opt.as_ref()?;
                            let nick_lower = nick.to_ascii_lowercase();
                            let ids = ns.identities.read().unwrap();
                            let identity = ids.get(&nick_lower)?;
                            // All five SCRAM fields must be present; accounts registered
                            // via keypair only have no password credentials.
                            let scram_stored_key = identity.scram_stored_key.clone()?;
                            let scram_server_key = identity.scram_server_key.clone()?;
                            let scram_salt = identity.scram_salt.clone()?;
                            let scram_iterations = identity.scram_iterations?;
                            let bcrypt_hash = identity.bcrypt_hash.clone()?;
                            Some(sasl::PasswordRecord {
                                account: nick.to_string(),
                                scram_stored_key,
                                scram_server_key,
                                scram_salt,
                                scram_iterations,
                                bcrypt_hash,
                            })
                        });

                        match sasl::new_session(&mechanism_name, lookup) {
                            Some(session) => {
                                sasl_session = Some(session);
                                // Send empty challenge to kick off the exchange.
                                let reply = format!(":{} AUTHENTICATE +", self.state.server_name());
                                send_raw(reply).await;
                            }
                            None => {
                                // Unsupported mechanism.
                                let nick = pending_nick.as_deref().unwrap_or("*");
                                let reply = IrcMessage::numeric(
                                    RPL_SASLMECHS,
                                    nick,
                                    &[sasl::SUPPORTED_MECHANISMS, "are available SASL mechanisms"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;
                                let reply = IrcMessage::numeric(
                                    ERR_SASLFAIL,
                                    nick,
                                    &["SASL authentication failed"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;
                            }
                        }
                    } else if sasl_session.is_some() {
                        let nick = pending_nick.as_deref().unwrap_or("*").to_string();
                        // Take ownership so we can move into spawn_blocking.
                        let mut session = sasl_session.take().unwrap();
                        let param_owned = param.to_string();
                        let step_result = tokio::task::spawn_blocking(move || {
                            let result = session.step(&param_owned);
                            (session, result)
                        })
                        .await
                        .expect("spawn_blocking panicked");
                        let (returned_session, result) = step_result;

                        match result {
                            Ok(SaslStep::Challenge(challenge)) => {
                                // Put the session back — exchange is not done yet.
                                sasl_session = Some(returned_session);
                                let reply = format!(
                                    ":{} AUTHENTICATE {}",
                                    self.state.server_name(),
                                    challenge
                                );
                                send_raw(reply).await;
                            }
                            Ok(SaslStep::Done { account }) => {
                                authenticated_account = Some(account.clone());
                                // sasl_session stays None (already taken)

                                let userhost =
                                    format!("{}!*@*", pending_nick.as_deref().unwrap_or("*"));
                                let reply = IrcMessage::numeric(
                                    RPL_LOGGEDIN,
                                    &nick,
                                    &[&userhost, &account, "You are now logged in as"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;

                                let reply = IrcMessage::numeric(
                                    RPL_SASLSUCCESS,
                                    &nick,
                                    &["SASL authentication successful"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;
                            }
                            Err(SaslError::AuthFailed) => {
                                // sasl_session stays None
                                let reply = IrcMessage::numeric(
                                    ERR_SASLFAIL,
                                    &nick,
                                    &["SASL authentication failed"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;
                            }
                            Err(SaslError::Malformed(msg_text)) => {
                                warn!(
                                    client_id = %self.id,
                                    error = %msg_text,
                                    "SASL malformed message"
                                );
                                let reply = IrcMessage::numeric(
                                    ERR_SASLFAIL,
                                    &nick,
                                    &["SASL authentication failed"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;
                            }
                            Err(SaslError::UnexpectedMessage) => {
                                let reply = IrcMessage::numeric(
                                    ERR_SASLABORTED,
                                    &nick,
                                    &["SASL authentication aborted"],
                                )
                                .with_prefix(self.state.server_name());
                                send_raw(reply.serialize()).await;
                            }
                        }
                    }
                }

                Command::Nick => {
                    if let Some(nick) = msg.params.first() {
                        pending_nick = Some(nick.clone());
                    } else {
                        let reply =
                            IrcMessage::numeric(ERR_NONICKNAMEGIVEN, "*", &["No nickname given"])
                                .with_prefix(self.state.server_name());
                        send_raw(reply.serialize()).await;
                    }
                }
                Command::User => {
                    if msg.params.len() >= 4 {
                        let username = msg.params[0].clone();
                        let realname = msg.params[3].clone();
                        pending_user = Some((username, realname));
                    } else {
                        let reply = IrcMessage::numeric(
                            ERR_NEEDMOREPARAMS,
                            "*",
                            &["USER", "Not enough parameters"],
                        )
                        .with_prefix(self.state.server_name());
                        send_raw(reply.serialize()).await;
                    }
                }
                Command::Pass => {
                    // Accept but ignore (SASL supersedes PASS-based auth).
                }
                Command::Ping => {
                    let token = msg.params.first().map(|s| s.as_str()).unwrap_or("");
                    let pong = IrcMessage::pong(token).with_prefix(self.state.server_name());
                    send_raw(pong.serialize()).await;
                }
                Command::Quit => {
                    return None;
                }
                _ => {
                    let reply =
                        IrcMessage::numeric(ERR_NOTREGISTERED, "*", &["You have not registered"])
                            .with_prefix(self.state.server_name());
                    send_raw(reply.serialize()).await;
                }
            }

            // Complete registration only when CAP negotiation is done AND
            // both NICK and USER have been received.
            if cap_negotiating {
                continue;
            }
            if let (Some(nick), Some((username, realname))) = (&pending_nick, &pending_user) {
                match self
                    .state
                    .register_client(
                        self.id,
                        nick,
                        username,
                        realname,
                        &self.hostname,
                        tx.clone(),
                        cancel.clone(),
                        authenticated_account.clone(),
                        pending_caps,
                    )
                    .await
                {
                    Ok(handle) => {
                        // Announce the new client to remote nodes via the relay bus.
                        self.state
                            .relay_publish(RelayEvent::ClientIntro {
                                client: handle.clone(),
                                node_id: self.state.relay().node_id().clone(),
                            })
                            .await;
                        send_welcome_burst(&self.state, &handle).await;
                        return Some(handle);
                    }
                    Err(crate::state::NickError::InUse) => {
                        let reply = IrcMessage::numeric(
                            ERR_NICKNAMEINUSE,
                            "*",
                            &[nick, "Nickname is already in use"],
                        )
                        .with_prefix(self.state.server_name());
                        send_raw(reply.serialize()).await;
                        pending_nick = None; // Let them try another nick.
                    }
                    Err(crate::state::NickError::Invalid) => {
                        let reply = IrcMessage::numeric(
                            ERR_ERRONEUSNICKNAME,
                            "*",
                            &[nick, "Erroneous nickname"],
                        )
                        .with_prefix(self.state.server_name());
                        send_raw(reply.serialize()).await;
                        pending_nick = None;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Welcome burst
// ---------------------------------------------------------------------------

/// Send the standard IRC welcome burst: 001–005 + LUSERS + MOTD.
async fn send_welcome_burst(state: &SharedState, client: &ClientHandle) {
    let server = state.server_name();
    let nick = &client.info.nick;

    client.send_numeric(
        RPL_WELCOME,
        &[&format!("Welcome to the AIRC Network, {nick}!")],
    );
    client.send_numeric(
        RPL_YOURHOST,
        &[&format!("Your host is {server}, running AIRC v0.1.0")],
    );
    client.send_numeric(
        RPL_CREATED,
        &["This server was created for agents and humans alike"],
    );
    // RPL_MYINFO: <server> <version> <user_modes> <channel_modes>
    client.send_numeric(RPL_MYINFO, &[server, "airc-0.1.0", "io", "imnstklv"]);

    // RPL_ISUPPORT (005) — advertise supported features.
    client.send_numeric(
        RPL_ISUPPORT,
        &[
            "CHANTYPES=#&",
            "PREFIX=(ov)@+",
            "CHANMODES=b,k,l,imnst",
            "NETWORK=AIRC",
            "CASEMAPPING=ascii",
            "are supported by this server",
        ],
    );

    // LUSERS — connection statistics.
    handler::send_lusers(state, client).await;

    handler::send_motd(state, client);
}

// ---------------------------------------------------------------------------
// Writer task (generic over any AsyncWrite)
// ---------------------------------------------------------------------------

/// Drains the outgoing channel and writes IRC lines to any async writer.
///
/// Batches multiple queued messages into a single buffer before calling
/// `write_all()`, reducing the number of syscalls under load.
///
/// Exits immediately if the `cancel` token is cancelled — this happens when
/// `ClientHandle::send_line` finds the outbound buffer full (slow client).
async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::Receiver<Arc<str>>,
    cancel: CancellationToken,
) {
    let mut buf = Vec::with_capacity(1024);

    loop {
        tokio::select! {
            biased;

            // Cancellation wins immediately — stop writing for this client.
            _ = cancel.cancelled() => break,

            maybe_line = rx.recv() => {
                let line = match maybe_line {
                    Some(l) => l,
                    None => break, // Channel closed (tx dropped after reader exit).
                };

                buf.clear();

                // Write the first message.
                buf.extend_from_slice(line.as_bytes());
                buf.extend_from_slice(b"\r\n");

                // Drain any additional queued messages without blocking.
                while let Ok(extra) = rx.try_recv() {
                    buf.extend_from_slice(extra.as_bytes());
                    buf.extend_from_slice(b"\r\n");
                }

                if writer.write_all(&buf).await.is_err() {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unexpected disconnect cleanup
// ---------------------------------------------------------------------------

/// Clean up when a client disconnects without sending QUIT.
async fn handle_unexpected_disconnect(state: &SharedState, client_id: ClientId) {
    let client = state.get_client(client_id).await;

    // Notify peers.
    let peers = state.peers_in_shared_channels(client_id).await;
    if let Some(ref client) = client {
        let quit_msg = IrcMessage::quit(Some("Connection closed")).with_prefix(client.prefix());
        for peer in &peers {
            peer.send_message_tagged(&quit_msg);
        }

        // Relay QUIT to remote nodes.
        state
            .relay_publish(RelayEvent::Quit {
                client_id,
                reason: Some("Connection closed".to_string()),
            })
            .await;
    }

    state.remove_client(client_id).await;
    if let Some(ref client) = client {
        warn!(
            client_id = %client_id,
            nick = %client.info.nick,
            "unexpected disconnect"
        );
    }
}
