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
use tokio_rustls::server::TlsStream;
use tracing::{debug, info, warn};

use airc_shared::reply::*;
use airc_shared::{Command, IrcMessage};

use crate::client::{ClientHandle, ClientId};
use crate::handler;
use crate::state::SharedState;

/// Size of the per-client outgoing message buffer.
const SEND_BUFFER: usize = 512;

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

        // Spawn the writer task for TCP.
        let writer_handle = tokio::spawn(write_loop(writer, rx));

        // Run the reader (registration + command dispatch).
        self.read_loop(BufReader::new(reader), tx).await;

        // Reader is done — the writer will finish once tx is dropped.
        let _ = writer_handle.await;

        info!(client_id = %self.id, "connection closed");
    }

    /// Run the connection over a TLS-wrapped TCP stream.
    pub async fn run_tls(self, stream: TlsStream<TcpStream>) {
        let (reader, writer) = tokio::io::split(stream);
        let (tx, rx) = mpsc::channel::<Arc<str>>(SEND_BUFFER);

        // Spawn the writer task for TLS.
        let writer_handle = tokio::spawn(write_loop(writer, rx));

        // Run the reader (registration + command dispatch).
        self.read_loop(BufReader::new(reader), tx).await;

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
    ///
    /// Returns when the reader hits EOF or the client sends QUIT.
    pub async fn run_generic<R: AsyncBufRead + Unpin + Send + 'static>(
        self,
        reader: R,
        tx: mpsc::Sender<Arc<str>>,
    ) {
        self.read_loop(reader, tx).await;
        info!(client_id = %self.id, "connection closed");
    }

    /// Read lines from a buffered reader, handle registration, then dispatch.
    async fn read_loop<R: AsyncBufRead + Unpin>(&self, mut reader: R, tx: mpsc::Sender<Arc<str>>) {
        // --- Registration phase ---
        let client = match self.registration_phase(&mut reader, &tx).await {
            Some(c) => c,
            None => return, // Connection closed or failed during registration.
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
    /// negotiation), validate, register, send welcome burst.
    ///
    /// CAP flow:
    /// - `CAP LS [302]` → reply `CAP * LS :` (empty list — no caps yet)
    /// - `CAP LIST`     → reply `CAP * LIST :`
    /// - `CAP REQ`      → reply `CAP * NAK :<caps>` (reject all for now)
    /// - `CAP END`      → clear `cap_negotiating`; complete registration if
    ///                    NICK+USER already received
    ///
    /// Registration is deferred until both CAP negotiation is finished AND
    /// NICK+USER have been received.
    async fn registration_phase<R: AsyncBufRead + Unpin>(
        &self,
        reader: &mut R,
        tx: &mpsc::Sender<Arc<str>>,
    ) -> Option<ClientHandle> {
        let mut pending_nick: Option<String> = None;
        let mut pending_user: Option<(String, String)> = None; // (username, realname)
        let mut cap_negotiating = false;
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
                            // Begin CAP negotiation.
                            cap_negotiating = true;
                            // Reply with empty capability list.
                            // Use the nick we have so far, or "*" if not yet known.
                            let nick = pending_nick.as_deref().unwrap_or("*");
                            let reply = format!(
                                ":{} CAP {} LS :",
                                self.state.server_name(),
                                nick
                            );
                            send_raw(reply).await;
                        }
                        Some("LIST") => {
                            let nick = pending_nick.as_deref().unwrap_or("*");
                            let reply = format!(
                                ":{} CAP {} LIST :",
                                self.state.server_name(),
                                nick
                            );
                            send_raw(reply).await;
                        }
                        Some("REQ") => {
                            // Reject all capability requests.
                            let nick = pending_nick.as_deref().unwrap_or("*");
                            let caps = msg.params.get(1).map(|s| s.as_str()).unwrap_or("");
                            let reply = format!(
                                ":{} CAP {} NAK :{}",
                                self.state.server_name(),
                                nick,
                                caps
                            );
                            send_raw(reply).await;
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
                    // Accept but ignore for now (future NickServ integration).
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
                    )
                    .await
                {
                    Ok(handle) => {
                        // Announce the new nick to remote nodes via the relay bus.
                        let nick_msg =
                            IrcMessage::nick(&handle.info.nick).with_prefix(handle.prefix());
                        self.state.relay_publish(&nick_msg).await;
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
            "CHANMODES=,,kl,imnst",
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
async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::Receiver<Arc<str>>) {
    let mut buf = Vec::with_capacity(1024);

    while let Some(line) = rx.recv().await {
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

// ---------------------------------------------------------------------------
// Unexpected disconnect cleanup
// ---------------------------------------------------------------------------

/// Clean up when a client disconnects without sending QUIT.
async fn handle_unexpected_disconnect(state: &SharedState, client_id: ClientId) {
    let client = state.get_client(client_id).await;

    // Log quit to all channels the user is in (before removing them).
    if let Some(ref client) = client {
        let channels = state.channels_for_client(client_id).await;
        for ch in &channels {
            state
                .logger()
                .log_quit(ch, &client.info.nick, "Connection closed");
        }
    }

    // Notify peers.
    let peers = state.peers_in_shared_channels(client_id).await;
    if let Some(ref client) = client {
        let quit_msg = IrcMessage::quit(Some("Connection closed")).with_prefix(client.prefix());
        let line: Arc<str> = quit_msg.serialize().into();
        for peer in &peers {
            peer.send_line(&line);
        }

        // Relay QUIT to remote nodes (they derive nick removal from this).
        state.relay_publish(&quit_msg).await;
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
