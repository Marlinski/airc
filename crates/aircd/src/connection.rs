//! Per-client connection lifecycle — reader, writer, and registration.
//!
//! Each TCP connection is managed by a [`Connection`]. It splits the socket
//! into a reader and writer, handles the IRC registration handshake
//! (NICK + USER → welcome burst), and then dispatches commands to the handler.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
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

    /// Run the connection to completion. Takes ownership of the TCP stream.
    pub async fn run(self, stream: TcpStream) {
        let (reader, writer) = stream.into_split();
        let (tx, rx) = mpsc::channel::<String>(SEND_BUFFER);

        // Spawn the writer task.
        let writer_handle = tokio::spawn(write_loop(writer, rx));

        // Run the reader (registration + command dispatch).
        self.read_loop(BufReader::new(reader), tx).await;

        // Reader is done — the writer will finish once tx is dropped.
        let _ = writer_handle.await;

        info!(client_id = %self.id, "connection closed");
    }

    /// Read lines from the socket, handle registration, then dispatch commands.
    async fn read_loop(
        &self,
        mut reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        tx: mpsc::Sender<String>,
    ) {
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

    /// Handle the registration handshake: wait for NICK + USER, validate,
    /// register, send welcome burst.
    async fn registration_phase(
        &self,
        reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
        tx: &mpsc::Sender<String>,
    ) -> Option<ClientHandle> {
        let mut pending_nick: Option<String> = None;
        let mut pending_user: Option<(String, String)> = None; // (username, realname)
        let mut line_buf = String::new();

        // Helper to send a raw line during pre-registration (no ClientHandle yet).
        let send_raw = |line: String| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(line).await;
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
                Command::Nick => {
                    if let Some(nick) = msg.params.first() {
                        pending_nick = Some(nick.clone());
                    } else {
                        let reply = IrcMessage::numeric(ERR_NONICKNAMEGIVEN, "*", &["No nickname given"])
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
                        let reply =
                            IrcMessage::numeric(ERR_NEEDMOREPARAMS, "*", &["USER", "Not enough parameters"])
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
                    let reply = IrcMessage::numeric(ERR_NOTREGISTERED, "*", &["You have not registered"])
                        .with_prefix(self.state.server_name());
                    send_raw(reply.serialize()).await;
                }
            }

            // Try to complete registration.
            if let (Some(nick), Some((username, realname))) = (&pending_nick, &pending_user) {
                match self
                    .state
                    .register_client(self.id, nick, username, realname, &self.hostname, tx.clone())
                    .await
                {
                    Ok(handle) => {
                        send_welcome_burst(&self.state, &handle);
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

/// Send the standard IRC welcome burst: 001–004 + MOTD.
fn send_welcome_burst(state: &SharedState, client: &ClientHandle) {
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
    client.send_numeric(RPL_MYINFO, &[server, "airc-0.1.0", "io", "itkln"]);

    handler::send_motd(state, client);
}

// ---------------------------------------------------------------------------
// Writer task
// ---------------------------------------------------------------------------

/// Drains the outgoing channel and writes lines to the socket.
async fn write_loop(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(line) = rx.recv().await {
        let mut buf = line.into_bytes();
        buf.extend_from_slice(b"\r\n");
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
        for peer in &peers {
            peer.send_message(&quit_msg);
        }
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
