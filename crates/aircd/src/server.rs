//! TCP accept loop — the heart of the AIRC server.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

use airc_shared::{Command, IrcMessage};

use crate::connection::Connection;
use crate::ipc;
use crate::relay::InboundEvent;
use crate::state::SharedState;

/// The AIRC IRC server.
pub struct Server {
    state: SharedState,
    tls_acceptor: Option<TlsAcceptor>,
}

impl Server {
    pub fn new(state: SharedState, tls_acceptor: Option<TlsAcceptor>) -> Self {
        Self {
            state,
            tls_acceptor,
        }
    }

    /// Bind and run the server. This function runs until the process is shut down.
    pub async fn run(&self) -> std::io::Result<()> {
        let addr = &self.state.config().bind_addr;
        let listener = TcpListener::bind(addr).await?;

        info!(addr = %addr, name = %self.state.server_name(), "AIRC server listening (plaintext)");

        // Optionally bind the TLS listener.
        let tls_listener = if self.tls_acceptor.is_some() {
            let tls_addr = self.state.config().tls_bind_addr().to_string();
            let tls_listener = TcpListener::bind(&tls_addr).await?;
            info!(addr = %tls_addr, "AIRC server listening (TLS)");
            Some(tls_listener)
        } else {
            None
        };

        // Start the IPC listener for `aircd stop` / `aircd status` commands.
        let (mut ipc_rx, ipc_sock_path) = match ipc::start_listener(self.state.clone()) {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "failed to start IPC listener (aircd stop will not work)");
                // Continue without IPC — the server still works, just no
                // graceful shutdown via `aircd stop`.
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                drop(tx); // closed immediately — ipc_rx will never yield
                (rx, ipc::socket_path())
            }
        };

        // Subscribe to inbound relay events from remote nodes.
        let mut relay_rx = match self.state.relay_subscribe().await {
            Ok(rx) => Some(rx),
            Err(e) => {
                warn!(error = %e, "failed to subscribe to relay events (single-instance mode)");
                None
            }
        };

        let result = async {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, peer_addr)) => {
                                // Disable Nagle's algorithm for low-latency delivery.
                                let _ = stream.set_nodelay(true);

                                let id = self.state.next_client_id();
                                let hostname = peer_addr.ip().to_string();
                                info!(client_id = %id, peer = %peer_addr, "new connection");

                                let conn = Connection::new(id, self.state.clone(), hostname);
                                tokio::spawn(async move {
                                    conn.run_tcp(stream).await;
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "failed to accept connection");
                            }
                        }
                    }

                    // TLS accept (only active if TLS is configured).
                    result = async {
                        match (&tls_listener, &self.tls_acceptor) {
                            (Some(l), Some(_)) => l.accept().await,
                            _ => std::future::pending().await,
                        }
                    } => {
                        match result {
                            Ok((stream, peer_addr)) => {
                                // Disable Nagle's algorithm for low-latency delivery.
                                let _ = stream.set_nodelay(true);

                                let acceptor = self.tls_acceptor.clone().unwrap();
                                let id = self.state.next_client_id();
                                let hostname = peer_addr.ip().to_string();
                                info!(client_id = %id, peer = %peer_addr, "new TLS connection");

                                let state = self.state.clone();
                                tokio::spawn(async move {
                                    match acceptor.accept(stream).await {
                                        Ok(tls_stream) => {
                                            let conn = Connection::new(id, state, hostname);
                                            conn.run_tls(tls_stream).await;
                                        }
                                        Err(e) => {
                                            warn!(client_id = %id, error = %e, "TLS handshake failed");
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                error!(error = %e, "failed to accept TLS connection");
                            }
                        }
                    }

                    // Inbound relay events from remote nodes.
                    Some(event) = async {
                        match relay_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        self.handle_relay_event(event).await;
                    }

                    // Shutdown signal from IPC (aircd stop).
                    Some(signal) = ipc_rx.recv() => {
                        match signal {
                            ipc::IpcSignal::Shutdown { reason } => {
                                info!(reason = %reason, "shutting down via IPC");
                                self.state.shutdown_all().await;
                                info!("server shut down gracefully (IPC)");
                                return Ok(());
                            }
                        }
                    }

                    _ = tokio::signal::ctrl_c() => {
                        info!("received shutdown signal, closing connections...");
                        self.state.shutdown_all().await;
                        info!("server shut down gracefully");
                        return Ok(());
                    }
                }
            }
        }
        .await;

        // Clean up IPC socket.
        ipc::cleanup(&ipc_sock_path);

        result
    }

    /// Process a single inbound relay event from a remote node.
    ///
    /// Dispatches on the IRC command to update local state and notify
    /// local clients. This is the only place with inbound dispatch logic —
    /// the relay bus is a dumb broadcast pipe.
    async fn handle_relay_event(&self, event: InboundEvent) {
        match event {
            InboundEvent::Message(relayed) => {
                let source = &relayed.source_node;
                let msg = &relayed.message;

                match &msg.command {
                    // -------------------------------------------------------
                    // NICK — a remote user changed nick (or just registered).
                    //
                    // The prefix carries the OLD identity (nick!user@host),
                    // and params[0] is the new nick.  For a fresh registration
                    // the prefix nick == new nick.
                    // -------------------------------------------------------
                    Command::Nick => {
                        let Some(new_nick) = msg.params.first() else {
                            return;
                        };

                        // Extract old nick from the prefix (if present).
                        if let Some(ref prefix) = msg.prefix {
                            let old_nick = prefix.split('!').next().unwrap_or(prefix);
                            if !old_nick.eq_ignore_ascii_case(new_nick) {
                                self.state.remove_remote_nick(old_nick).await;
                            }
                        }

                        self.state.add_remote_nick(new_nick, source.clone()).await;

                        // Notify local peers that share channels with the old nick.
                        // (Channel membership key updates happen implicitly when
                        // the remote node relays JOIN/PART.)
                        let line: Arc<str> = msg.serialize().into();
                        let clients = self.state.all_local_clients().await;
                        for client in &clients {
                            // Only send to clients who would see this nick
                            // (i.e. share a channel). For now broadcast to all
                            // local clients — a refinement can filter later.
                            // TODO: Filter to peers in shared channels.
                            client.send_line(&line);
                        }
                    }

                    // -------------------------------------------------------
                    // QUIT — a remote user disconnected.
                    // -------------------------------------------------------
                    Command::Quit => {
                        let nick = msg
                            .prefix
                            .as_ref()
                            .map(|p| p.split('!').next().unwrap_or(p).to_string());

                        if let Some(ref nick) = nick {
                            // Remove from channels first, then nick registry.
                            self.state.remove_remote_nick_from_all_channels(nick).await;
                            self.state.remove_remote_nick(nick).await;
                        }

                        // Notify local peers.
                        let line: Arc<str> = msg.serialize().into();
                        let clients = self.state.all_local_clients().await;
                        for client in &clients {
                            client.send_line(&line);
                        }
                    }

                    // -------------------------------------------------------
                    // JOIN — a remote user joined a channel.
                    // -------------------------------------------------------
                    Command::Join => {
                        let Some(channel_name) = msg.params.first() else {
                            return;
                        };
                        let nick = msg
                            .prefix
                            .as_ref()
                            .map(|p| p.split('!').next().unwrap_or(p).to_string());
                        let Some(ref nick) = nick else { return };

                        self.state
                            .add_remote_channel_member(channel_name, nick, source.clone())
                            .await;

                        // Notify local members of that channel.
                        if let Some(members) = self.state.channel_members(channel_name).await {
                            let line: Arc<str> = msg.serialize().into();
                            for member in &members {
                                member.send_line(&line);
                            }
                        }
                    }

                    // -------------------------------------------------------
                    // PART — a remote user left a channel.
                    // -------------------------------------------------------
                    Command::Part => {
                        let Some(channel_name) = msg.params.first() else {
                            return;
                        };
                        let nick = msg
                            .prefix
                            .as_ref()
                            .map(|p| p.split('!').next().unwrap_or(p).to_string());
                        let Some(ref nick) = nick else { return };

                        // Notify local members BEFORE removing (so they see the PART).
                        if let Some(members) = self.state.channel_members(channel_name).await {
                            let line: Arc<str> = msg.serialize().into();
                            for member in &members {
                                member.send_line(&line);
                            }
                        }

                        self.state
                            .remove_remote_channel_member(channel_name, nick)
                            .await;
                    }

                    // -------------------------------------------------------
                    // PRIVMSG / NOTICE — message to a channel or nick.
                    // -------------------------------------------------------
                    Command::Privmsg | Command::Notice => {
                        let Some(target) = msg.params.first() else {
                            return;
                        };

                        if airc_shared::validate::is_channel_name(target) {
                            // Channel message — deliver to local members.
                            if let Some(members) = self.state.channel_members(target).await {
                                let line: Arc<str> = msg.serialize().into();
                                for member in &members {
                                    member.send_line(&line);
                                }
                            }
                        } else {
                            // DM — deliver to the local nick.
                            if let Some(client) = self.state.find_client_by_nick(target).await {
                                client.send_message(msg);
                            }
                        }
                    }

                    // -------------------------------------------------------
                    // KICK — a remote op kicked someone from a channel.
                    // -------------------------------------------------------
                    Command::Kick => {
                        let Some(channel_name) = msg.params.first() else {
                            return;
                        };
                        let target_nick = msg.params.get(1);

                        // Notify local members (including the kicked user).
                        if let Some(members) = self.state.channel_members(channel_name).await {
                            let line: Arc<str> = msg.serialize().into();
                            for member in &members {
                                member.send_line(&line);
                            }
                        }

                        // Remove from channel membership.
                        if let Some(nick) = target_nick {
                            self.state
                                .remove_remote_channel_member(channel_name, nick)
                                .await;
                        }
                    }

                    // -------------------------------------------------------
                    // TOPIC — channel topic changed on a remote node.
                    // -------------------------------------------------------
                    Command::Topic => {
                        let Some(channel_name) = msg.params.first() else {
                            return;
                        };
                        let new_topic = msg.params.get(1).cloned().unwrap_or_default();
                        let setter = msg
                            .prefix
                            .as_ref()
                            .map(|p| p.split('!').next().unwrap_or(p).to_string())
                            .unwrap_or_default();
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();

                        // Update local channel state.
                        if let Some(members) = self
                            .state
                            .set_channel_topic(channel_name, new_topic, setter, now)
                            .await
                        {
                            let line: Arc<str> = msg.serialize().into();
                            for member in &members {
                                member.send_line(&line);
                            }
                        }
                    }

                    // -------------------------------------------------------
                    // MODE — channel mode changed on a remote node.
                    // -------------------------------------------------------
                    Command::Mode => {
                        let Some(channel_name) = msg.params.first() else {
                            return;
                        };

                        // Only handle channel modes relayed from remote nodes.
                        if !airc_shared::validate::is_channel_name(channel_name) {
                            return;
                        }

                        // Apply mode changes to local state.
                        if let Some(mode_str) = msg.params.get(1) {
                            let mut param_idx = 2;
                            let mut setting = true;
                            for ch in mode_str.chars() {
                                match ch {
                                    '+' => setting = true,
                                    '-' => setting = false,
                                    'o' => {
                                        if let Some(target_nick) = msg.params.get(param_idx) {
                                            self.state
                                                .set_channel_operator(
                                                    channel_name,
                                                    target_nick,
                                                    setting,
                                                )
                                                .await;
                                            param_idx += 1;
                                        }
                                    }
                                    'k' => {
                                        let param = if setting {
                                            let p = msg.params.get(param_idx).map(|s| s.as_str());
                                            param_idx += 1;
                                            p
                                        } else {
                                            None
                                        };
                                        self.state
                                            .set_channel_mode(channel_name, 'k', setting, param)
                                            .await;
                                    }
                                    'l' => {
                                        let param = if setting {
                                            let p = msg.params.get(param_idx).map(|s| s.as_str());
                                            param_idx += 1;
                                            p
                                        } else {
                                            None
                                        };
                                        self.state
                                            .set_channel_mode(channel_name, 'l', setting, param)
                                            .await;
                                    }
                                    flag @ ('i' | 't' | 'n') => {
                                        self.state
                                            .set_channel_mode(channel_name, flag, setting, None)
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                        }

                        // Notify local channel members.
                        if let Some(members) = self.state.channel_members(channel_name).await {
                            let line: Arc<str> = msg.serialize().into();
                            for member in &members {
                                member.send_line(&line);
                            }
                        }
                    }

                    // Unhandled commands — ignore silently.
                    _ => {
                        warn!(
                            source_node = %source,
                            command = ?msg.command,
                            "relay: ignoring unhandled command"
                        );
                    }
                }
            }
            InboundEvent::NodeUp { node_id, nicks } => {
                info!(
                    node_id = %node_id,
                    nick_count = nicks.len(),
                    "relay: remote node came online"
                );
                for nick in &nicks {
                    self.state.add_remote_nick(nick, node_id.clone()).await;
                }
            }
            InboundEvent::NodeDown { node_id } => {
                info!(node_id = %node_id, "relay: remote node went offline");

                // Collect local peers who share channels with the departing
                // remote nicks *before* we remove them from state.
                let local_peers = self.state.local_peers_of_node(&node_id).await;

                let removed_nicks = self.state.remove_node(&node_id).await;

                // Broadcast a netsplit QUIT to all affected local clients.
                if !removed_nicks.is_empty() && !local_peers.is_empty() {
                    let netsplit_reason = format!("{} {}", self.state.server_name(), node_id);
                    let quit_msg =
                        IrcMessage::quit(Some(&netsplit_reason)).with_prefix(node_id.to_string());
                    let line: Arc<str> = quit_msg.serialize().into();
                    for peer in &local_peers {
                        peer.send_line(&line);
                    }
                }

                info!(
                    removed_nicks = removed_nicks.len(),
                    "relay: cleaned up nicks from departed node"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
impl Server {
    /// Construct a server without TLS for use in integration tests.
    pub fn new_for_test(state: SharedState) -> Self {
        Self {
            state,
            tls_acceptor: None,
        }
    }

    /// Expose the private `handle_relay_event` to test modules.
    pub async fn handle_relay_event_for_test(&self, event: InboundEvent) {
        self.handle_relay_event(event).await;
    }
}
