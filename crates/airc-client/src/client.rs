//! High-level IRC client with auto-reconnect.
//!
//! [`IrcClient`] is the main entry point. It connects to a server, handles
//! registration, and provides simple async methods for channel operations
//! and message fetching.
//!
//! When the connection drops, the client automatically reconnects with
//! exponential backoff, re-registers, re-joins channels, and flushes any
//! messages that were queued during the disconnection.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{info, warn};

use airc_shared::IrcMessage;

use crate::config::ClientConfig;
use crate::conn::{self, LineSender};
use crate::error::ClientError;
use crate::event::{ChannelMessage, IrcEvent};
use crate::state::{ChannelStatus, ClientState};

/// Backoff parameters for auto-reconnect.
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(1);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);
const RECONNECT_BACKOFF_FACTOR: u32 = 2;

/// An IRC client connection with auto-reconnect.
///
/// Cheap to clone — all state is behind `Arc`. Cloning gives another handle
/// to the same connection.
#[derive(Debug, Clone)]
pub struct IrcClient {
    /// Sender for outgoing IRC lines — swapped on reconnect.
    line_tx: Arc<RwLock<LineSender>>,
    /// Shared client state (channels, messages, identity).
    state: ClientState,
    /// Config snapshot.
    config: ClientConfig,
    /// Queue for messages sent while disconnected.
    send_queue: Arc<Mutex<VecDeque<String>>>,
    /// Whether we're currently connected (false during reconnect).
    connected: Arc<RwLock<bool>>,
}

impl IrcClient {
    /// Connect to an IRC server and complete registration.
    ///
    /// This will:
    /// 1. Open a TCP connection
    /// 2. Send NICK/USER (and PASS if configured)
    /// 3. Wait for RPL_WELCOME (001) or timeout after 10 seconds
    /// 4. Collect the server MOTD (if any)
    /// 5. Auto-join any channels from config
    ///
    /// Returns the connected client, the MOTD lines, and an event receiver for
    /// real-time events.
    pub async fn connect(
        config: ClientConfig,
    ) -> Result<(Self, Vec<String>, mpsc::Receiver<IrcEvent>), ClientError> {
        let (line_tx, mut event_rx, state) = conn::connect(&config).await?;

        // Wait for registration to complete (RPL_WELCOME).
        let timeout = Duration::from_secs(10);
        let registered = tokio::time::timeout(timeout, async {
            loop {
                match event_rx.recv().await {
                    Some(IrcEvent::Registered { nick, server, .. }) => {
                        info!(nick = %nick, server = %server, "registered with IRC server");
                        return Ok(());
                    }
                    Some(IrcEvent::Disconnected { reason }) => {
                        return Err(ClientError::Registration(reason));
                    }
                    Some(_) => {
                        // Ignore other events during registration.
                        continue;
                    }
                    None => {
                        return Err(ClientError::Registration(
                            "event channel closed".to_string(),
                        ));
                    }
                }
            }
        })
        .await;

        match registered {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(ClientError::Timeout),
        }

        // Collect MOTD lines. The server sends them right after RPL_WELCOME.
        // We drain events until we see MotdEnd (376) or hit a short timeout
        // (some servers may not send a MOTD at all).
        let mut motd_lines = Vec::new();
        let mut pending_events = Vec::new();
        let motd_timeout = Duration::from_secs(3);
        let _ = tokio::time::timeout(motd_timeout, async {
            loop {
                match event_rx.recv().await {
                    Some(IrcEvent::Motd { line }) => {
                        motd_lines.push(line);
                    }
                    Some(IrcEvent::MotdEnd) => {
                        break;
                    }
                    Some(other) => {
                        // Buffer non-MOTD events that arrive during this window.
                        pending_events.push(other);
                    }
                    None => break,
                }
            }
        })
        .await;

        let client = IrcClient {
            line_tx: Arc::new(RwLock::new(line_tx)),
            state,
            config: config.clone(),
            send_queue: Arc::new(Mutex::new(VecDeque::new())),
            connected: Arc::new(RwLock::new(true)),
        };

        // Auto-join channels.
        for channel in &config.auto_join {
            if let Err(e) = client.join(channel).await {
                warn!(channel = %channel, error = %e, "failed to auto-join channel");
            }
        }

        // Set up the reconnect bridge: forward events from the internal
        // receiver to an external one, intercepting Disconnected to trigger
        // reconnect.
        let (ext_tx, ext_rx) = mpsc::channel::<IrcEvent>(512);

        // Re-emit any events that arrived during the MOTD collection window.
        for ev in pending_events {
            let _ = ext_tx.send(ev).await;
        }

        let reconnect_client = client.clone();
        tokio::spawn(reconnect_bridge(reconnect_client, event_rx, ext_tx));

        Ok((client, motd_lines, ext_rx))
    }

    // -- Channel operations ---------------------------------------------------

    /// Join an IRC channel.
    pub async fn join(&self, channel: &str) -> Result<(), ClientError> {
        self.send_line(&IrcMessage::join(channel).serialize()).await
    }

    /// Leave an IRC channel.
    pub async fn part(&self, channel: &str, reason: Option<&str>) -> Result<(), ClientError> {
        self.send_line(&IrcMessage::part(channel, reason).serialize())
            .await
    }

    /// Send a message to a channel or user.
    pub async fn say(&self, target: &str, text: &str) -> Result<(), ClientError> {
        self.send_line(&IrcMessage::privmsg(target, text).serialize())
            .await
    }

    /// Send a notice to a channel or user.
    pub async fn notice(&self, target: &str, text: &str) -> Result<(), ClientError> {
        self.send_line(&IrcMessage::notice(target, text).serialize())
            .await
    }

    /// Disconnect from the server.
    pub async fn quit(&self, reason: Option<&str>) -> Result<(), ClientError> {
        self.send_line(&IrcMessage::quit(reason).serialize()).await
    }

    // -- Message fetching (the key agent UX) ---------------------------------

    /// Fetch unread messages from a specific channel.
    ///
    /// Advances the read cursor so the same messages won't be returned again.
    pub async fn fetch(&self, channel: &str) -> Vec<ChannelMessage> {
        self.state.fetch(channel).await
    }

    /// Fetch unread messages from all channels, sorted by timestamp.
    pub async fn fetch_all(&self) -> Vec<ChannelMessage> {
        self.state.fetch_all().await
    }

    /// Fetch the last N messages from a channel (does NOT advance cursor).
    pub async fn fetch_last(&self, channel: &str, n: usize) -> Vec<ChannelMessage> {
        self.state.fetch_last(channel, n).await
    }

    // -- NickServ helpers ----------------------------------------------------

    /// Identify with NickServ using a password.
    pub async fn nickserv_identify(&self, password: &str) -> Result<(), ClientError> {
        self.say("NickServ", &format!("IDENTIFY {password}")).await
    }

    /// Register with NickServ.
    pub async fn nickserv_register(&self, password: &str) -> Result<(), ClientError> {
        self.say("NickServ", &format!("REGISTER {password}")).await
    }

    // -- Status / queries ----------------------------------------------------

    /// Get our current nickname.
    pub async fn nick(&self) -> String {
        self.state.nick().await
    }

    /// Get the list of joined channels.
    pub async fn channels(&self) -> Vec<String> {
        self.state.channels().await
    }

    /// Get status summary: channels, unread counts, member counts.
    pub async fn status(&self) -> Vec<ChannelStatus> {
        self.state.status().await
    }

    /// Whether we're registered with the server.
    pub async fn is_registered(&self) -> bool {
        self.state.is_registered().await
    }

    /// Whether the client is currently connected.
    pub async fn is_connected(&self) -> bool {
        *self.connected.read().await
    }

    /// Access the underlying state for advanced queries.
    pub fn state(&self) -> &ClientState {
        &self.state
    }

    /// Access the config.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    // -- Low-level -----------------------------------------------------------

    /// Send a raw IRC line (without \r\n).
    ///
    /// If disconnected, the line is queued and will be sent after reconnection.
    pub async fn send_line(&self, line: &str) -> Result<(), ClientError> {
        let connected = *self.connected.read().await;
        if connected {
            let tx = self.line_tx.read().await;
            match tx.send(line.to_string()).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    // Channel closed — connection is dead. Queue the message.
                    drop(tx);
                    self.send_queue.lock().await.push_back(line.to_string());
                    Ok(())
                }
            }
        } else {
            // Queue while disconnected.
            self.send_queue.lock().await.push_back(line.to_string());
            Ok(())
        }
    }
}

/// Bridge task: forwards events from the internal conn event_rx to the
/// external ext_tx. When a Disconnected event arrives, it triggers the
/// auto-reconnect loop, then resumes forwarding from the new connection.
async fn reconnect_bridge(
    client: IrcClient,
    mut event_rx: mpsc::Receiver<IrcEvent>,
    ext_tx: mpsc::Sender<IrcEvent>,
) {
    loop {
        // Forward events until disconnected or channel closes.
        loop {
            match event_rx.recv().await {
                Some(IrcEvent::Disconnected { ref reason }) => {
                    info!(reason = %reason, "connection lost, starting auto-reconnect");
                    *client.connected.write().await = false;
                    // Forward the Disconnected event to the caller.
                    let _ = ext_tx
                        .send(IrcEvent::Disconnected {
                            reason: reason.clone(),
                        })
                        .await;
                    break;
                }
                Some(ev) => {
                    if ext_tx.send(ev).await.is_err() {
                        // External receiver dropped — stop entirely.
                        return;
                    }
                }
                None => {
                    // Internal channel closed (connection dead).
                    *client.connected.write().await = false;
                    let _ = ext_tx
                        .send(IrcEvent::Disconnected {
                            reason: "event channel closed".to_string(),
                        })
                        .await;
                    break;
                }
            }
        }

        // --- Auto-reconnect loop with exponential backoff ---
        let mut delay = RECONNECT_INITIAL_DELAY;
        let mut attempt: u32 = 0;

        // Remember which channels we were in so we can re-join.
        let channels_to_rejoin = client.state.channels().await;

        loop {
            attempt += 1;
            info!(
                attempt,
                delay_secs = delay.as_secs(),
                "attempting reconnect"
            );
            let _ = ext_tx.send(IrcEvent::Reconnecting { attempt }).await;

            tokio::time::sleep(delay).await;

            // Try to establish a new connection.
            match conn::connect(&client.config).await {
                Ok((new_line_tx, new_event_rx, _new_state)) => {
                    // We reuse the existing ClientState (preserving buffered
                    // messages), but swap the line sender.
                    *client.line_tx.write().await = new_line_tx;
                    *client.connected.write().await = true;
                    client.state.set_registered().await;

                    // Wait briefly for registration to complete.
                    event_rx = new_event_rx;
                    let reg_timeout = Duration::from_secs(10);
                    let reg_ok = tokio::time::timeout(reg_timeout, async {
                        loop {
                            match event_rx.recv().await {
                                Some(IrcEvent::Registered { nick, server, .. }) => {
                                    info!(nick = %nick, server = %server, "re-registered after reconnect");
                                    client.state.set_nick(nick).await;
                                    client.state.set_server_name(server).await;
                                    return true;
                                }
                                Some(IrcEvent::Disconnected { reason }) => {
                                    warn!(reason = %reason, "disconnected during reconnect registration");
                                    return false;
                                }
                                Some(_) => continue,
                                None => return false,
                            }
                        }
                    })
                    .await;

                    match reg_ok {
                        Ok(true) => {
                            // Re-join channels.
                            for ch in &channels_to_rejoin {
                                let join_msg = IrcMessage::join(ch).serialize();
                                let tx = client.line_tx.read().await;
                                let _ = tx.send(join_msg).await;
                            }

                            // Flush the send queue.
                            let mut queue = client.send_queue.lock().await;
                            let tx = client.line_tx.read().await;
                            while let Some(line) = queue.pop_front() {
                                if tx.send(line).await.is_err() {
                                    break;
                                }
                            }
                            drop(queue);
                            drop(tx);

                            info!("reconnected successfully");
                            let _ = ext_tx.send(IrcEvent::Reconnected).await;

                            // Break out of the reconnect loop — resume
                            // forwarding events from the new event_rx.
                            break;
                        }
                        _ => {
                            warn!("registration failed after reconnect, retrying");
                            *client.connected.write().await = false;
                            // Fall through to retry.
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, attempt, "reconnect attempt failed");
                }
            }

            // Exponential backoff.
            delay = (delay * RECONNECT_BACKOFF_FACTOR).min(RECONNECT_MAX_DELAY);
        }
    }
}
