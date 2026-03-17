//! Relay integration: publish events to remote nodes and build/apply state
//! snapshots for `NodeUp` synchronisation.

use std::sync::Arc;

use crate::channel::Channel;
use crate::client::Client;
use crate::relay::{RelayEvent, SnapshotChannel, SnapshotClient, SnapshotMembership};

use super::{SharedState, fnv1a_hash};

impl SharedState {
    // -- Relay notifications ------------------------------------------------

    /// Publish a typed S2S event to all remote nodes.
    ///
    /// Fire-and-forget — relay failures are logged but never fail the caller.
    pub async fn relay_publish(&self, event: RelayEvent) {
        if let Err(e) = self.inner.relay.publish(event).await {
            tracing::warn!(error = %e, "relay: publish failed");
        }
    }

    /// Subscribe to inbound relay events. Returns the receiver for the
    /// server's select loop.
    pub async fn relay_subscribe(
        &self,
    ) -> Result<tokio::sync::mpsc::Receiver<RelayEvent>, crate::relay::RelayError> {
        self.inner.relay.subscribe().await
    }

    // -- State snapshot (running-state burst for NodeUp) --------------------

    /// Build a compact snapshot of all local running state for export to a
    /// newly-joined node.
    ///
    /// Collects all locally-connected clients, all channels with at least one
    /// local member, and all membership records.  Uses a stable `channel_id`
    /// derived from a FNV-1a hash of the lowercase channel name so the
    /// receiving node can re-associate memberships with channels without
    /// needing a central registry.
    pub async fn build_state_snapshot(
        &self,
        target_node_id: crate::client::NodeId,
    ) -> crate::relay::RelayEvent {
        // Snapshot clients — local only; remote clients will provide their own
        // snapshots when they handle the same NodeUp.
        let snapshot_clients: Vec<SnapshotClient> = {
            let mut clients = Vec::new();
            for shard_lock in &self.inner.users {
                let shard = shard_lock.read().await;
                for c in shard.values() {
                    if c.is_local() {
                        clients.push(SnapshotClient {
                            client_id: c.id,
                            info: (*c.info).clone(),
                            node_id: self.inner.relay.node_id().clone(),
                        });
                    }
                }
            }
            clients
        };

        // Snapshot channels and memberships together under a single channels
        // read lock to ensure consistency.
        let mut snapshot_channels: Vec<SnapshotChannel> = Vec::new();
        let mut snapshot_memberships: Vec<SnapshotMembership> = Vec::new();

        {
            let channels_map = self.inner.channels.read().await;
            for (_key, arc) in channels_map.iter() {
                let ch = arc.read().await;
                // Only include channels that have at least one local member.
                let has_local = {
                    // Group member IDs by shard and check each shard.
                    let mut found = false;
                    'outer: for mid in ch.members.keys() {
                        let shard = self.user_shard(*mid).read().await;
                        if shard.get(mid).is_some_and(|c| c.is_local()) {
                            found = true;
                            break 'outer;
                        }
                    }
                    found
                };
                if !has_local {
                    continue;
                }

                let channel_id = fnv1a_hash(&ch.name.to_ascii_lowercase());

                snapshot_channels.push(SnapshotChannel {
                    channel_id,
                    name: ch.name.clone(),
                    topic: ch.topic.clone(),
                    modes: ch.modes.clone(),
                    created_at: ch.created_at,
                });

                for (member_id, membership) in &ch.members {
                    snapshot_memberships.push(SnapshotMembership {
                        client_id: *member_id,
                        channel_id,
                        mode: membership.mode,
                    });
                }
            }
        }

        RelayEvent::StateSnapshot {
            target_node_id,
            clients: snapshot_clients,
            channels: snapshot_channels,
            memberships: snapshot_memberships,
        }
    }

    /// Apply an inbound `StateSnapshot` to local running state.
    ///
    /// Idempotent: re-inserting a client or channel that already exists is
    /// harmless (the existing entry wins).  Called only when
    /// `target_node_id == self.node_id()`.
    pub async fn apply_state_snapshot(
        &self,
        clients: Vec<SnapshotClient>,
        channels: Vec<SnapshotChannel>,
        memberships: Vec<SnapshotMembership>,
    ) {
        // Build a channel_id → Arc<RwLock<Channel>> map as we upsert channels.
        // We need this to wire up memberships without re-acquiring the outer map lock.
        let mut channel_id_to_key: std::collections::HashMap<u64, String> =
            std::collections::HashMap::new();

        // -- Insert channels ------------------------------------------------
        for sc in channels {
            let key = sc.name.to_ascii_lowercase();
            channel_id_to_key.insert(sc.channel_id, key.clone());

            let arc = {
                let mut map = self.inner.channels.write().await;
                map.entry(key.clone())
                    .or_insert_with(|| {
                        let mut ch = Channel::new(sc.name.clone());
                        ch.topic = sc.topic;
                        ch.modes = sc.modes;
                        ch.created_at = sc.created_at;
                        Arc::new(tokio::sync::RwLock::new(ch))
                    })
                    .clone()
            };
            drop(arc); // arc held only to ensure entry exists; no further mutation needed here
        }

        // -- Insert clients -------------------------------------------------
        for sc in clients {
            let nick_lower = sc.info.nick.to_ascii_lowercase();
            let id = sc.client_id;
            // Skip if already known (e.g. we got a ClientIntro earlier).
            {
                let shard = self.user_shard(id).read().await;
                if shard.contains_key(&id) {
                    continue;
                }
            }
            let client = Client::new_remote(id, Arc::new(sc.info), sc.node_id);
            self.user_shard(id).write().await.insert(id, client);
            self.inner.nick_index.write().await.insert(nick_lower, id);
        }

        // -- Wire memberships -----------------------------------------------
        for sm in memberships {
            let key = match channel_id_to_key.get(&sm.channel_id) {
                Some(k) => k.clone(),
                None => continue, // channel not in snapshot — skip
            };

            let ch_arc = match self.inner.channels.read().await.get(&key).cloned() {
                Some(a) => a,
                None => continue,
            };

            {
                let mut ch = ch_arc.write().await;
                if !ch.is_member(sm.client_id) {
                    ch.add_member(sm.client_id);
                    // Apply member mode.
                    match sm.mode {
                        crate::channel::MemberMode::Op => {
                            ch.set_operator(sm.client_id, true);
                        }
                        crate::channel::MemberMode::Voice => {
                            ch.set_voice(sm.client_id, true);
                        }
                        crate::channel::MemberMode::Normal => {}
                    }
                }
            }

            // Update membership_index.
            self.inner
                .membership_index
                .write()
                .await
                .entry(sm.client_id)
                .or_default()
                .insert(key);
        }
    }
}
