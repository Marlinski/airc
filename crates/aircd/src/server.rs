//! TCP accept loop — the heart of the AIRC server.

use std::collections::HashMap;

use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use airc_shared::IrcMessage;

use crate::connection::Connection;
use crate::ipc;
use crate::relay::RelayEvent;
use crate::state::SharedState;

/// Capacity of the gossip channel between `PersistentState` and the relay
/// publish task.  Gossip is best-effort — blobs dropped when full are
/// recovered by anti-entropy on reconnect (e.g. after a Redis outage).
const GOSSIP_CHANNEL_CAPACITY: usize = 2048;

/// How often the periodic anti-entropy timer fires.
///
/// Every tick each node broadcasts its per-key CRDT hashes to all peers.
/// Peers compare against their own state and reply with any diverged blobs.
/// This catches any gossip deltas that were silently dropped by Redis pub/sub.
const ANTI_ENTROPY_INTERVAL: Duration = Duration::from_secs(5 * 60);

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
        //
        // Events are forwarded through an unbounded channel to a dedicated
        // handler task (ASYNC-2).  This decouples relay processing from the
        // accept loop: heavy work (anti-entropy merges, netsplit cleanup) runs
        // in a separate task and cannot stall new TCP accepts.
        let relay_rx = match self.state.relay_subscribe().await {
            Ok(rx) => Some(rx),
            Err(e) => {
                warn!(error = %e, "failed to subscribe to relay events (single-instance mode)");
                None
            }
        };

        let (relay_event_tx, mut relay_event_rx) = mpsc::unbounded_channel::<RelayEvent>();

        if let Some(mut rx) = relay_rx {
            let tx = relay_event_tx.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if tx.send(event).is_err() {
                        break; // receiver dropped — server is shutting down
                    }
                }
            });
        }

        let relay_state = self.state.clone();
        tokio::spawn(async move {
            let server_handle = Server {
                state: relay_state,
                tls_acceptor: None,
            };
            while let Some(event) = relay_event_rx.recv().await {
                // AntiEntropyResponse can merge hundreds of CRDT blobs; spawn
                // it so it does not block subsequent relay events (ASYNC-3).
                if matches!(event, RelayEvent::AntiEntropyResponse { .. }) {
                    let s = server_handle.state.clone();
                    tokio::spawn(async move {
                        if let RelayEvent::AntiEntropyResponse { from_node, blobs } = event {
                            debug!(from_node = %from_node, crdt_count = blobs.len(), "relay: applying anti-entropy response (spawned)");
                            if let Some(ps) = s.persistent() {
                                for (crdt_id, blob) in &blobs {
                                    ps.merge_crdt(crdt_id, blob).await;
                                }
                            }
                        }
                    });
                } else {
                    server_handle.handle_relay_event(event).await;
                }
            }
        });

        // Wire gossip: bounded channel — PersistentState writes into it on
        // every CRDT mutation; a background task forwards blobs to the relay.
        // Bounded to avoid unbounded memory growth during Redis outages; dropped
        // blobs are recovered by anti-entropy when connectivity resumes.
        if let Some(ps) = self.state.persistent() {
            let (gossip_tx, mut gossip_rx) =
                mpsc::channel::<(String, Vec<u8>)>(GOSSIP_CHANNEL_CAPACITY);
            ps.set_gossip_tx(gossip_tx);

            let relay_state = self.state.clone();
            tokio::spawn(async move {
                while let Some((crdt_id, payload)) = gossip_rx.recv().await {
                    if let Err(e) = relay_state
                        .relay()
                        .publish(RelayEvent::CrdtDelta {
                            crdt_id: crdt_id.clone(),
                            payload,
                        })
                        .await
                    {
                        warn!(crdt_id = %crdt_id, error = %e, "gossip: failed to publish CRDT delta");
                    }
                }
            });
        }

        // Periodic anti-entropy interval — only meaningful when PersistentState
        // is wired (i.e. Redis relay mode).  In NoopRelay / single-instance mode
        // we use `pending()` so the select arm never fires.
        let mut anti_entropy_interval: Option<time::Interval> = if self.state.persistent().is_some()
        {
            let mut iv = time::interval(ANTI_ENTROPY_INTERVAL);
            iv.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            // Skip the immediate first tick so we don't fire right at startup
            // (NodeUp anti-entropy already covers the connect-time sync).
            iv.tick().await;
            Some(iv)
        } else {
            None
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

                    // Inbound relay events are handled by a dedicated task
                    // (spawned above) and no longer processed here (ASYNC-2).

                    // Periodic anti-entropy: broadcast our CRDT hashes so peers
                    // can detect and repair any state that diverged due to
                    // dropped gossip messages.
                    _ = async {
                        match anti_entropy_interval.as_mut() {
                            Some(iv) => iv.tick().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Some(ps) = self.state.persistent() {
                            debug!("periodic anti-entropy: broadcasting CRDT hashes");
                            let hashes = ps.all_crdt_hashes().await;
                            let local_node = self.state.relay().node_id().to_string();
                            if let Err(e) = self
                                .state
                                .relay()
                                .publish(RelayEvent::AntiEntropyRequest {
                                    from_node: local_node,
                                    hashes,
                                })
                                .await
                            {
                                warn!(error = %e, "periodic anti-entropy: failed to publish request");
                            }
                        }
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
    /// Dispatches on the typed `RelayEvent` variant to update local state
    /// and notify local clients.  This is the only place with inbound
    /// dispatch logic — the relay bus is a dumb broadcast pipe.
    async fn handle_relay_event(&self, event: RelayEvent) {
        match event {
            // ---------------------------------------------------------------
            // ClientIntro — a new remote user has registered on another node.
            // ---------------------------------------------------------------
            RelayEvent::ClientIntro { client, node_id } => {
                debug!(
                    client_id = %client.id,
                    nick = %client.info.nick,
                    source_node = %node_id,
                    "relay: remote client registered"
                );
                // Always register as Remote, regardless of the ClientKind
                // carried in `client`.  In-process relays (e.g. PairRelay in
                // tests) forward the original Local handle; coercing here
                // ensures correctness across all relay backends.
                let remote = crate::client::Client::new_remote(client.id, client.info, node_id);
                self.state.add_remote_client(remote).await;
            }

            // ---------------------------------------------------------------
            // ClientDown — a remote user disconnected.
            // ---------------------------------------------------------------
            RelayEvent::ClientDown { client_id } => {
                debug!(client_id = %client_id, "relay: remote client disconnected");
                self.state.remove_remote_client(client_id).await;
            }

            // ---------------------------------------------------------------
            // NickChange — a remote user changed their nick.
            // ---------------------------------------------------------------
            RelayEvent::NickChange {
                client_id,
                new_nick,
            } => {
                // Look up the old nick before we mutate state, so we can
                // build a properly-prefixed NICK message for local peers.
                let old_prefix = {
                    let users = self.state.get_client(client_id).await;
                    users.map(|c| c.prefix())
                };

                self.state.update_client_nick(client_id, &new_nick).await;

                // Notify all local peers who share channels with this client.
                let peers = self.state.peers_in_shared_channels(client_id).await;
                if !peers.is_empty() {
                    let nick_msg = IrcMessage {
                        tags: vec![],
                        prefix: old_prefix,
                        command: airc_shared::Command::Nick,
                        params: vec![new_nick],
                    };
                    for peer in &peers {
                        peer.send_message_tagged(&nick_msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Join — a remote user joined a channel.
            // ---------------------------------------------------------------
            RelayEvent::Join { client_id, channel } => {
                // Look up client info to build the JOIN message prefix.
                let client_info = self.state.get_client(client_id).await;

                let (_snapshot, local_members) = self.state.join_channel(client_id, &channel).await;

                if !local_members.is_empty() {
                    let prefix = client_info.map(|c| c.prefix());
                    let join_msg = IrcMessage {
                        tags: vec![],
                        prefix,
                        command: airc_shared::Command::Join,
                        params: vec![channel],
                    };
                    for member in &local_members {
                        member.send_message_tagged(&join_msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Part — a remote user left a channel.
            // ---------------------------------------------------------------
            RelayEvent::Part {
                client_id,
                channel,
                reason,
            } => {
                // Look up prefix before removal so we can build the message.
                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());

                // Notify local members BEFORE removing (so they still see the PART).
                if let Some(members) = self.state.channel_members(&channel).await {
                    let mut params = vec![channel.clone()];
                    if let Some(r) = reason {
                        params.push(r);
                    }
                    let part_msg = IrcMessage {
                        tags: vec![],
                        prefix,
                        command: airc_shared::Command::Part,
                        params,
                    };
                    for member in &members {
                        member.send_message_tagged(&part_msg);
                    }
                }

                self.state.part_channel(client_id, &channel).await;
            }

            // ---------------------------------------------------------------
            // Quit — a remote user quit.
            // ---------------------------------------------------------------
            RelayEvent::Quit { client_id, reason } => {
                // Look up prefix before removal.
                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());

                // Collect local peers in shared channels before removing.
                let peers = self.state.peers_in_shared_channels(client_id).await;

                // Remove from all state.
                self.state.remove_remote_client(client_id).await;

                // Notify affected local peers.
                if !peers.is_empty() {
                    let mut params = vec![];
                    if let Some(r) = reason {
                        params.push(r);
                    }
                    let quit_msg = IrcMessage {
                        tags: vec![],
                        prefix,
                        command: airc_shared::Command::Quit,
                        params,
                    };
                    for peer in &peers {
                        peer.send_message_tagged(&quit_msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Privmsg — channel or DM message from a remote user.
            // ---------------------------------------------------------------
            RelayEvent::Privmsg {
                client_id,
                target,
                text,
            } => {
                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());
                let msg = IrcMessage {
                    tags: vec![],
                    prefix,
                    command: airc_shared::Command::Privmsg,
                    params: vec![target.clone(), text],
                };

                if airc_shared::validate::is_channel_name(&target) {
                    // Channel message — deliver to local members.
                    if let Some(members) = self.state.channel_members(&target).await {
                        for member in &members {
                            member.send_message_tagged(&msg);
                        }
                    }
                } else {
                    // DM — deliver to the local nick.
                    if let Some(client) = self.state.find_client_by_nick(&target).await {
                        client.send_message_tagged(&msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Notice — same fan-out logic as Privmsg.
            // ---------------------------------------------------------------
            RelayEvent::Notice {
                client_id,
                target,
                text,
            } => {
                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());
                let msg = IrcMessage {
                    tags: vec![],
                    prefix,
                    command: airc_shared::Command::Notice,
                    params: vec![target.clone(), text],
                };

                if airc_shared::validate::is_channel_name(&target) {
                    if let Some(members) = self.state.channel_members(&target).await {
                        for member in &members {
                            member.send_message_tagged(&msg);
                        }
                    }
                } else {
                    if let Some(client) = self.state.find_client_by_nick(&target).await {
                        client.send_message_tagged(&msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Topic — channel topic changed on a remote node.
            // ---------------------------------------------------------------
            RelayEvent::Topic {
                client_id,
                channel,
                text,
            } => {
                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());

                let setter = prefix
                    .as_deref()
                    .map(|p| p.split('!').next().unwrap_or(p).to_string())
                    .unwrap_or_default();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                // Update local channel state and notify local members.
                if let Some(members) = self
                    .state
                    .set_channel_topic(&channel, text.clone(), setter, now)
                    .await
                {
                    let topic_msg = IrcMessage {
                        tags: vec![],
                        prefix,
                        command: airc_shared::Command::Topic,
                        params: vec![channel, text],
                    };
                    for member in &members {
                        member.send_message_tagged(&topic_msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Mode — channel mode changed on a remote node.
            // ---------------------------------------------------------------
            RelayEvent::Mode {
                client_id,
                target,
                mode_string,
            } => {
                // Only handle channel modes.
                if !airc_shared::validate::is_channel_name(&target) {
                    return;
                }

                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());

                // Parse and apply mode changes to local state.
                // The mode_string may be a compact representation like "+o nick +k key".
                // We split it into the flags token and individual params.
                let parts: Vec<&str> = mode_string.splitn(2, ' ').collect();
                let flags = parts[0];
                // Remaining words are mode params (if any).
                let mode_params: Vec<&str> = if parts.len() > 1 {
                    parts[1].split_whitespace().collect()
                } else {
                    vec![]
                };

                let mut param_idx = 0usize;
                let mut setting = true;
                for ch in flags.chars() {
                    match ch {
                        '+' => setting = true,
                        '-' => setting = false,
                        'o' => {
                            if let Some(target_nick) = mode_params.get(param_idx) {
                                self.state
                                    .set_channel_operator(&target, target_nick, setting)
                                    .await;
                                param_idx += 1;
                            }
                        }
                        'k' => {
                            let param = if setting {
                                let p = mode_params.get(param_idx).copied();
                                param_idx += 1;
                                p
                            } else {
                                None
                            };
                            self.state
                                .set_channel_mode(&target, 'k', setting, param)
                                .await;
                        }
                        'l' => {
                            let param = if setting {
                                let p = mode_params.get(param_idx).copied();
                                param_idx += 1;
                                p
                            } else {
                                None
                            };
                            self.state
                                .set_channel_mode(&target, 'l', setting, param)
                                .await;
                        }
                        flag @ ('i' | 't' | 'n' | 'm' | 's') => {
                            self.state
                                .set_channel_mode(&target, flag, setting, None)
                                .await;
                        }
                        _ => {}
                    }
                }

                // Notify local channel members.
                if let Some(members) = self.state.channel_members(&target).await {
                    let mode_msg = IrcMessage {
                        tags: vec![],
                        prefix,
                        command: airc_shared::Command::Mode,
                        params: vec![target, mode_string],
                    };
                    for member in &members {
                        member.send_message_tagged(&mode_msg);
                    }
                }
            }

            // ---------------------------------------------------------------
            // Kick — a remote op kicked someone from a channel.
            // ---------------------------------------------------------------
            RelayEvent::Kick {
                client_id,
                channel,
                target_client_id,
                reason,
            } => {
                let prefix = self.state.get_client(client_id).await.map(|c| c.prefix());

                // Look up the target's nick for the KICK message.
                let target_nick = self
                    .state
                    .get_client(target_client_id)
                    .await
                    .map(|c| c.info.nick.clone())
                    .unwrap_or_default();

                // Notify local members (including the kicked user) BEFORE removing.
                if let Some(members) = self.state.channel_members(&channel).await {
                    let kick_msg = IrcMessage {
                        tags: vec![],
                        prefix,
                        command: airc_shared::Command::Kick,
                        params: vec![channel.clone(), target_nick, reason],
                    };
                    for member in &members {
                        member.send_message_tagged(&kick_msg);
                    }
                }

                // Remove the kicked client from the channel.
                self.state.part_channel(target_client_id, &channel).await;
            }

            // ---------------------------------------------------------------
            // NodeUp — a remote node came online; trigger anti-entropy and
            // publish a running-state snapshot so the new node gets our
            // connected clients and channel memberships immediately.
            // ---------------------------------------------------------------
            RelayEvent::NodeUp { node_id } => {
                info!(node_id = %node_id, "relay: remote node came online");

                // Trigger CRDT anti-entropy: send our hashes so the new node
                // can request any blobs it is missing.
                if let Some(ps) = self.state.persistent() {
                    let hashes = ps.all_crdt_hashes().await;
                    let local_node = self.state.relay().node_id().to_string();
                    if let Err(e) = self
                        .state
                        .relay()
                        .publish(RelayEvent::AntiEntropyRequest {
                            from_node: local_node,
                            hashes,
                        })
                        .await
                    {
                        warn!(error = %e, "relay: failed to publish anti-entropy request");
                    }
                }

                // Publish a running-state snapshot targeted at the new node so
                // it learns about all of our locally-connected clients and
                // channel memberships.  The snapshot is broadcast to the relay
                // channel but the receiving side only applies it when
                // target_node_id matches its own node ID.
                let snapshot = self.state.build_state_snapshot(node_id.clone()).await;
                let client_count = if let RelayEvent::StateSnapshot { ref clients, .. } = snapshot {
                    clients.len()
                } else {
                    0
                };
                debug!(
                    target_node = %node_id,
                    clients = client_count,
                    "relay: publishing state snapshot to new node"
                );
                if let Err(e) = self.state.relay().publish(snapshot).await {
                    warn!(error = %e, "relay: failed to publish state snapshot");
                }
            }

            // ---------------------------------------------------------------
            // NodeDown — a remote node went offline; netsplit cleanup.
            // ---------------------------------------------------------------
            RelayEvent::NodeDown { node_id } => {
                info!(node_id = %node_id, "relay: remote node went offline");

                // Collect local peers who share channels with the departing
                // remote clients *before* we remove them from state.
                let local_peers = self.state.local_peers_of_node(&node_id).await;

                let removed_clients = self.state.remove_node(&node_id).await;

                // Broadcast a netsplit QUIT to all affected local clients.
                if !removed_clients.is_empty() && !local_peers.is_empty() {
                    let netsplit_reason = format!("{} {}", self.state.server_name(), node_id);
                    for removed in &removed_clients {
                        let quit_msg =
                            IrcMessage::quit(Some(&netsplit_reason)).with_prefix(removed.prefix());
                        for peer in &local_peers {
                            peer.send_message_tagged(&quit_msg);
                        }
                    }
                }

                info!(
                    removed_clients = removed_clients.len(),
                    "relay: cleaned up clients from departed node"
                );
            }

            // -------------------------------------------------------------------
            // CrdtDelta — a peer published a CRDT mutation; merge it locally.
            // -------------------------------------------------------------------
            RelayEvent::CrdtDelta { crdt_id, payload } => {
                debug!(crdt_id = %crdt_id, bytes = payload.len(), "relay: applying incoming CRDT delta");
                if let Some(ps) = self.state.persistent() {
                    ps.merge_crdt(&crdt_id, &payload).await;
                }
            }

            // -------------------------------------------------------------------
            // AntiEntropyRequest — a peer sent its hashes; respond with
            // any blobs for CRDTs where our hash differs (or the peer lacks).
            // -------------------------------------------------------------------
            RelayEvent::AntiEntropyRequest { from_node, hashes } => {
                debug!(from_node = %from_node, crdt_count = hashes.len(), "relay: received anti-entropy request");
                let Some(ps) = self.state.persistent() else {
                    return;
                };

                let our_hashes = ps.all_crdt_hashes().await;
                let mut diverged: HashMap<String, Vec<u8>> = HashMap::new();

                // Export any CRDT where the peer's hash differs or is absent.
                for (crdt_id, our_hash) in &our_hashes {
                    let needs_send = match hashes.get(crdt_id) {
                        Some(peer_hash) => peer_hash != our_hash,
                        None => true, // peer is missing this CRDT entirely
                    };
                    if needs_send && let Some(blob) = ps.export_crdt(crdt_id).await {
                        diverged.insert(crdt_id.clone(), blob);
                    }
                }

                if !diverged.is_empty() {
                    let local_node = self.state.relay().node_id().to_string();
                    debug!(from_node = %from_node, sending = diverged.len(), "relay: sending anti-entropy response");
                    if let Err(e) = self
                        .state
                        .relay()
                        .publish(RelayEvent::AntiEntropyResponse {
                            from_node: local_node,
                            blobs: diverged,
                        })
                        .await
                    {
                        warn!(error = %e, "relay: failed to publish anti-entropy response");
                    }
                }
            }

            // -------------------------------------------------------------------
            // AntiEntropyResponse — peer sent blobs for diverged CRDTs; merge all.
            // -------------------------------------------------------------------
            RelayEvent::AntiEntropyResponse { from_node, blobs } => {
                debug!(from_node = %from_node, crdt_count = blobs.len(), "relay: applying anti-entropy response");
                let Some(ps) = self.state.persistent() else {
                    return;
                };
                for (crdt_id, blob) in &blobs {
                    ps.merge_crdt(crdt_id, blob).await;
                }
            }

            // -------------------------------------------------------------------
            // StateSnapshot — an existing node sent us its running state.
            //
            // Only apply it if we are the target.  All other nodes ignore it.
            // Spawned into its own task so a large snapshot (100k+ clients) does
            // not block subsequent relay events.
            // -------------------------------------------------------------------
            RelayEvent::StateSnapshot {
                target_node_id,
                clients,
                channels,
                memberships,
            } => {
                let our_node = self.state.relay().node_id().clone();
                if target_node_id != our_node {
                    return; // not for us
                }
                debug!(
                    clients = clients.len(),
                    channels = channels.len(),
                    memberships = memberships.len(),
                    "relay: applying state snapshot from remote node"
                );
                let s = self.state.clone();
                tokio::spawn(async move {
                    s.apply_state_snapshot(clients, channels, memberships).await;
                });
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
    pub async fn handle_relay_event_for_test(&self, event: RelayEvent) {
        self.handle_relay_event(event).await;
    }
}
