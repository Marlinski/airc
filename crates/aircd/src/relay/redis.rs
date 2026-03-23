//! Redis Pub/Sub relay — used in multi-instance (clustered) mode.
//!
//! All nodes publish to and subscribe from a single Redis channel
//! (`airc:relay`). Each message is a `RelayEnvelope` encoded as binary
//! protobuf using prost.
//!
//! # Heartbeat / NodeUp / NodeDown
//!
//! On `subscribe()`:
//! 1. A `NodeUp` envelope is published immediately.
//! 2. A heartbeat key `airc:heartbeat:<node_id>` is set with a 15-second TTL.
//! 3. A background task refreshes the heartbeat every 5 seconds.
//! 4. A separate watcher task scans for disappeared heartbeat keys and emits
//!    `NodeDown` when a previously-seen node's key expires.
//!
//! # Logging
//!
//! `RedisRelay` does **not** log. In multi-node mode, logging is handled by
//! `aircd-redis-logger` — a dedicated sidecar process that subscribes to
//! `airc:relay` and writes CSV logs.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use prost::Message as ProstMessage;
use rand::Rng;
use redis::AsyncCommands;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info, warn};

use airc_shared::relay::{
    AntiEntropyRequest as ProtoAeRequest, AntiEntropyResponse as ProtoAeResponse, ClientDown,
    ClientIntro as ProtoClientIntro, CrdtDelta as ProtoCrdtDelta, Join, Kick, Mode, NickChange,
    NodeDown as ProtoNodeDown, NodeUp as ProtoNodeUp, Notice, Part, Privmsg, Quit, RelayEnvelope,
    Topic,
};
use airc_shared::relay_proto::{
    SnapshotChannel as ProtoSnapshotChannel, SnapshotClient as ProtoSnapshotClient,
    SnapshotMembership as ProtoSnapshotMembership, StateSnapshot as ProtoStateSnapshot,
};
// Alias the proto-generated oneof enum to avoid clashing with crate::relay::RelayEvent.
use airc_shared::relay_proto::relay_envelope::Event as ProtoEvent;

use crate::channel::{ChannelModes, MemberMode};
use crate::client::{Client, ClientId, ClientInfo, NodeId};
use crate::relay::{
    BoxFuture, Relay, RelayError, RelayEvent, SnapshotChannel, SnapshotClient, SnapshotMembership,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Heartbeat key TTL in seconds.
const HEARTBEAT_TTL_SECS: u64 = 15;
/// Interval between heartbeat refreshes.
const HEARTBEAT_INTERVAL_SECS: u64 = 5;
/// Interval between node-down checks in the watcher task.
const NODEWATCH_INTERVAL_SECS: u64 = 6;
/// Inbound event channel buffer.
const INBOUND_BUF: usize = 4096;
/// Timeout for a single pub/sub read before treating connection as stale.
const PUBSUB_READ_TIMEOUT_SECS: u64 = 30;
/// Initial backoff on subscriber reconnect.
const RECONNECT_BACKOFF_MIN_MS: u64 = 500;
/// Maximum backoff on subscriber reconnect.
const RECONNECT_BACKOFF_MAX_MS: u64 = 30_000;

use airc_shared::relay::HEARTBEAT_KEY_PREFIX;
use airc_shared::relay::RELAY_CHANNEL;

// ---------------------------------------------------------------------------
// RedisRelay
// ---------------------------------------------------------------------------

/// Multi-node relay backed by Redis Pub/Sub with binary protobuf encoding.
pub struct RedisRelay {
    node_id: NodeId,
    client: redis::Client,
    /// Persistent multiplexed connection reused across all publish calls.
    /// Initialized lazily in `subscribe()`, so it is always ready before any
    /// `publish()` call reaches the hot path.
    conn: tokio::sync::OnceCell<redis::aio::MultiplexedConnection>,
}

impl RedisRelay {
    /// Connect to Redis and create the relay.
    pub fn new(redis_url: &str) -> Result<Self, RelayError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| RelayError::Transport(format!("redis connect: {e}")))?;

        let mut rng = rand::thread_rng();
        let id: String = (0..16)
            .map(|_| format!("{:02x}", rng.r#gen::<u8>()))
            .collect();
        let node_id = NodeId(id);

        Ok(Self {
            node_id,
            client,
            conn: tokio::sync::OnceCell::new(),
        })
    }

    // -- Internal helpers ---------------------------------------------------

    /// Get (or initialize) the persistent multiplexed connection.
    async fn get_conn(&self) -> Result<redis::aio::MultiplexedConnection, RelayError> {
        let conn = self
            .conn
            .get_or_try_init(|| async {
                self.client
                    .get_multiplexed_async_connection()
                    .await
                    .map_err(|e| RelayError::Transport(format!("redis conn: {e}")))
            })
            .await?;
        Ok(conn.clone())
    }

    /// Encode a `RelayEnvelope` and publish it to Redis.
    async fn publish_envelope(
        conn: &mut redis::aio::MultiplexedConnection,
        envelope: RelayEnvelope,
    ) -> Result<(), RelayError> {
        let bytes = envelope.encode_to_vec();
        conn.publish::<_, _, i64>(RELAY_CHANNEL, bytes.as_slice())
            .await
            .map_err(|e| RelayError::Transport(format!("redis publish: {e}")))?;
        Ok(())
    }

    /// Build a `RelayEnvelope` with `node_id` set and the given proto event.
    fn wrap(&self, event: ProtoEvent) -> RelayEnvelope {
        RelayEnvelope {
            node_id: self.node_id.0.clone(),
            event: Some(event),
        }
    }
}

// ---------------------------------------------------------------------------
// Relay impl
// ---------------------------------------------------------------------------

impl Relay for RedisRelay {
    fn node_id(&self) -> &NodeId {
        &self.node_id
    }

    fn publish(&self, event: RelayEvent) -> BoxFuture<'_, Result<(), RelayError>> {
        let envelope = match event {
            RelayEvent::ClientIntro { client, .. } => {
                self.wrap(ProtoEvent::ClientIntro(ProtoClientIntro {
                    client_id: client.id.0.to_string(),
                    nick: client.info.nick.clone(),
                    user: client.info.username.clone(),
                    host: client.info.hostname.clone(),
                }))
            }

            RelayEvent::ClientDown { client_id } => self.wrap(ProtoEvent::ClientDown(ClientDown {
                client_id: client_id.0.to_string(),
                reason: String::new(),
            })),

            RelayEvent::NickChange {
                client_id,
                new_nick,
            } => self.wrap(ProtoEvent::NickChange(NickChange {
                client_id: client_id.0.to_string(),
                new_nick,
            })),

            RelayEvent::Join { client_id, channel } => self.wrap(ProtoEvent::Join(Join {
                client_id: client_id.0.to_string(),
                channel,
            })),

            RelayEvent::Part {
                client_id,
                channel,
                reason,
            } => self.wrap(ProtoEvent::Part(Part {
                client_id: client_id.0.to_string(),
                channel,
                reason: reason.unwrap_or_default(),
            })),

            RelayEvent::Quit { client_id, reason } => self.wrap(ProtoEvent::Quit(Quit {
                client_id: client_id.0.to_string(),
                reason: reason.unwrap_or_default(),
            })),

            RelayEvent::Privmsg {
                client_id,
                target,
                text,
            } => {
                use airc_shared::relay_proto::privmsg::Target;
                let target_field = if target.starts_with('#') || target.starts_with('&') {
                    Target::TargetChannel(target)
                } else {
                    Target::TargetClientId(target)
                };
                self.wrap(ProtoEvent::Privmsg(Privmsg {
                    client_id: client_id.0.to_string(),
                    target: Some(target_field),
                    text,
                }))
            }

            RelayEvent::Notice {
                client_id,
                target,
                text,
            } => {
                use airc_shared::relay_proto::notice::Target;
                let target_field = if target.starts_with('#') || target.starts_with('&') {
                    Target::TargetChannel(target)
                } else {
                    Target::TargetClientId(target)
                };
                self.wrap(ProtoEvent::Notice(Notice {
                    client_id: client_id.0.to_string(),
                    target: Some(target_field),
                    text,
                }))
            }

            RelayEvent::Topic {
                client_id,
                channel,
                text,
            } => self.wrap(ProtoEvent::Topic(Topic {
                client_id: client_id.0.to_string(),
                channel,
                text,
            })),

            RelayEvent::Mode {
                client_id,
                target,
                mode_string,
            } => self.wrap(ProtoEvent::Mode(Mode {
                client_id: client_id.0.to_string(),
                target,
                mode_string,
                params: vec![],
            })),

            RelayEvent::Kick {
                client_id,
                channel,
                target_client_id,
                reason,
            } => self.wrap(ProtoEvent::Kick(Kick {
                client_id: client_id.0.to_string(),
                channel,
                target_client_id: target_client_id.0.to_string(),
                reason,
            })),

            RelayEvent::CrdtDelta { crdt_id, payload } => {
                self.wrap(ProtoEvent::CrdtDelta(ProtoCrdtDelta { crdt_id, payload }))
            }

            RelayEvent::AntiEntropyRequest { from_node, hashes } => {
                self.wrap(ProtoEvent::AntiEntropyRequest(ProtoAeRequest {
                    from_node,
                    hashes,
                }))
            }

            RelayEvent::AntiEntropyResponse { from_node, blobs } => {
                self.wrap(ProtoEvent::AntiEntropyResponse(ProtoAeResponse {
                    from_node,
                    blobs,
                }))
            }

            RelayEvent::NodeUp { node_id } => {
                self.wrap(ProtoEvent::NodeUp(ProtoNodeUp { node_id: node_id.0 }))
            }

            RelayEvent::NodeDown { node_id } => {
                self.wrap(ProtoEvent::NodeDown(ProtoNodeDown { node_id: node_id.0 }))
            }

            RelayEvent::StateSnapshot {
                target_node_id,
                clients,
                channels,
                memberships,
            } => {
                let proto_clients = clients
                    .into_iter()
                    .map(|c| ProtoSnapshotClient {
                        client_id: c.client_id.0,
                        nick: c.info.nick.clone(),
                        user: c.info.username.clone(),
                        host: c.info.hostname.clone(),
                        modes: c.info.modes,
                        away: c.info.away.unwrap_or_default(),
                    })
                    .collect();

                let proto_channels = channels
                    .into_iter()
                    .map(|ch| {
                        let (topic_text, topic_setter, topic_ts) =
                            ch.topic.unwrap_or_default();
                        let mut mode_flags: u32 = 0;
                        if ch.modes.invite_only {
                            mode_flags |= 1 << 0;
                        }
                        if ch.modes.topic_locked {
                            mode_flags |= 1 << 1;
                        }
                        if ch.modes.no_external {
                            mode_flags |= 1 << 2;
                        }
                        if ch.modes.moderated {
                            mode_flags |= 1 << 3;
                        }
                        if ch.modes.secret {
                            mode_flags |= 1 << 4;
                        }
                        ProtoSnapshotChannel {
                            channel_id: ch.channel_id,
                            name: ch.name,
                            topic_text,
                            topic_setter,
                            topic_ts,
                            mode_flags,
                            mode_key: ch.modes.key.unwrap_or_default(),
                            mode_limit: ch.modes.limit.unwrap_or(0) as u32,
                            created_at: ch.created_at,
                        }
                    })
                    .collect();

                let proto_memberships = memberships
                    .into_iter()
                    .map(|m| ProtoSnapshotMembership {
                        client_id: m.client_id.0,
                        channel_id: m.channel_id,
                        mode: match m.mode {
                            MemberMode::Normal => 0,
                            MemberMode::Voice => 1,
                            MemberMode::Op => 2,
                        },
                    })
                    .collect();

                self.wrap(ProtoEvent::StateSnapshot(ProtoStateSnapshot {
                    target_node_id: target_node_id.0,
                    clients: proto_clients,
                    channels: proto_channels,
                    memberships: proto_memberships,
                }))
            }
        };

        Box::pin(async move {
            let mut conn = self.get_conn().await?;
            Self::publish_envelope(&mut conn, envelope).await
        })
    }

    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<RelayEvent>, RelayError>> {
        let client = self.client.clone();
        let own_node_id = self.node_id.0.clone();

        Box::pin(async move {
            let (tx, rx) = mpsc::channel::<RelayEvent>(INBOUND_BUF);

            // Eagerly initialize the persistent publish connection so it is
            // ready before any publish() call arrives on the hot path.
            let mut pub_conn = self.get_conn().await?;

            // -- Heartbeat key -----------------------------------------------
            {
                let heartbeat_key = format!("{HEARTBEAT_KEY_PREFIX}{own_node_id}");
                pub_conn
                    .set_ex::<_, _, ()>(&heartbeat_key, "1", HEARTBEAT_TTL_SECS)
                    .await
                    .map_err(|e| RelayError::Transport(format!("redis heartbeat set: {e}")))?;
            }

            // -- Publish NodeUp ----------------------------------------------
            {
                let env = RelayEnvelope {
                    node_id: own_node_id.clone(),
                    event: Some(ProtoEvent::NodeUp(ProtoNodeUp {
                        node_id: own_node_id.clone(),
                    })),
                };
                let bytes = env.encode_to_vec();
                pub_conn
                    .publish::<_, _, i64>(RELAY_CHANNEL, bytes.as_slice())
                    .await
                    .map_err(|e| RelayError::Transport(format!("redis publish NodeUp: {e}")))?;
            }

            // -- Heartbeat refresh task (reuses the shared publish connection) -
            {
                let mut hb_conn = pub_conn.clone();
                let hb_key = format!("{HEARTBEAT_KEY_PREFIX}{own_node_id}");
                tokio::spawn(async move {
                    let mut interval = time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
                    loop {
                        interval.tick().await;
                        if let Err(e) = hb_conn
                            .set_ex::<_, _, ()>(&hb_key, "1", HEARTBEAT_TTL_SECS)
                            .await
                        {
                            warn!("redis heartbeat refresh failed: {e}");
                        }
                    }
                });
            }

            // -- Node-down watcher task --------------------------------------
            {
                let watch_client = client.clone();
                let watch_tx = tx.clone();
                tokio::spawn(async move {
                    let mut known_nodes: HashSet<String> = HashSet::new();
                    let mut interval = time::interval(Duration::from_secs(NODEWATCH_INTERVAL_SECS));
                    let mut conn = match watch_client.get_multiplexed_async_connection().await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!("redis node-watch init conn failed: {e}");
                            return;
                        }
                    };
                    loop {
                        interval.tick().await;
                        let pattern = format!("{HEARTBEAT_KEY_PREFIX}*");
                        // Use SCAN instead of KEYS to avoid blocking Redis on large keyspaces.
                        let alive: HashSet<String> = {
                            let mut scan_stream = match conn.scan_match::<_, String>(&pattern).await
                            {
                                Ok(s) => s,
                                Err(e) => {
                                    warn!("redis node-watch scan failed: {e}");
                                    continue;
                                }
                            };
                            let mut keys = HashSet::new();
                            while let Some(key) = scan_stream.next_item().await {
                                if let Some(id) = key.strip_prefix(HEARTBEAT_KEY_PREFIX) {
                                    keys.insert(id.to_string());
                                }
                            }
                            keys
                        };

                        for gone in known_nodes.difference(&alive) {
                            debug!("node down detected: {gone}");
                            let event = RelayEvent::NodeDown {
                                node_id: NodeId(gone.clone()),
                            };
                            if watch_tx.send(event).await.is_err() {
                                return;
                            }
                        }
                        // NodeUp from new heartbeat keys is handled by the
                        // subscriber task (which receives the NodeUp envelope).
                        known_nodes = alive;
                    }
                });
            }

            // -- Subscriber task (reconnects with exponential backoff) -------
            {
                let sub_client = client.clone();
                let sub_tx = tx.clone();
                let own_id = own_node_id.clone();
                tokio::spawn(async move {
                    let mut backoff_ms = RECONNECT_BACKOFF_MIN_MS;
                    loop {
                        // Attempt to (re-)connect and subscribe.
                        let pub_sub_conn = match sub_client.get_async_pubsub().await {
                            Ok(c) => c,
                            Err(e) => {
                                error!(
                                    "redis pubsub connect failed: {e}; retrying in {backoff_ms}ms"
                                );
                                time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms = (backoff_ms * 2).min(RECONNECT_BACKOFF_MAX_MS);
                                continue;
                            }
                        };
                        let mut pub_sub_conn = pub_sub_conn;
                        if let Err(e) = pub_sub_conn.subscribe(RELAY_CHANNEL).await {
                            error!("redis subscribe failed: {e}; retrying in {backoff_ms}ms");
                            time::sleep(Duration::from_millis(backoff_ms)).await;
                            backoff_ms = (backoff_ms * 2).min(RECONNECT_BACKOFF_MAX_MS);
                            continue;
                        }

                        info!("redis pubsub connected");
                        backoff_ms = RECONNECT_BACKOFF_MIN_MS; // reset backoff on successful connect

                        use futures_util::StreamExt;
                        let mut stream = pub_sub_conn.on_message();
                        loop {
                            // Timeout each read so a dead connection doesn't hang forever.
                            let msg = match time::timeout(
                                Duration::from_secs(PUBSUB_READ_TIMEOUT_SECS),
                                stream.next(),
                            )
                            .await
                            {
                                Ok(Some(m)) => m,
                                Ok(None) => {
                                    warn!("redis pubsub stream ended; reconnecting");
                                    break; // reconnect outer loop
                                }
                                Err(_) => {
                                    warn!("redis pubsub read timeout; reconnecting");
                                    break; // reconnect outer loop
                                }
                            };
                            let payload: Vec<u8> = match msg.get_payload() {
                                Ok(p) => p,
                                Err(e) => {
                                    warn!("redis msg payload error: {e}");
                                    continue;
                                }
                            };
                            let envelope = match RelayEnvelope::decode(payload.as_slice()) {
                                Ok(e) => e,
                                Err(e) => {
                                    warn!("relay envelope decode error: {e}");
                                    continue;
                                }
                            };

                            // Skip own messages.
                            if envelope.node_id == own_id {
                                continue;
                            }

                            let source_node = NodeId(envelope.node_id.clone());

                            let event = match envelope_to_relay_event(envelope, source_node) {
                                Some(ev) => ev,
                                None => continue,
                            };
                            if sub_tx.send(event).await.is_err() {
                                // Receiver dropped — server is shutting down.
                                return;
                            }
                        }
                    }
                });
            }

            Ok(rx)
        })
    }
}

// ---------------------------------------------------------------------------
// Envelope → RelayEvent
// ---------------------------------------------------------------------------

/// Convert a decoded `RelayEnvelope` into a typed [`RelayEvent`].
/// Returns `None` for unknown/empty events.
fn envelope_to_relay_event(envelope: RelayEnvelope, source_node: NodeId) -> Option<RelayEvent> {
    use airc_shared::relay_proto::notice::Target as NoticeTarget;
    use airc_shared::relay_proto::privmsg::Target as PrivmsgTarget;

    match envelope.event? {
        ProtoEvent::ClientIntro(ci) => {
            let id = parse_client_id(&ci.client_id)?;
            let info = Arc::new(ClientInfo {
                nick: ci.nick,
                username: ci.user,
                realname: String::new(),
                hostname: ci.host,
                registered: true,
                identified: false,
                account: None,
                modes: 0,
                away: None,
                caps: 0,
            });
            let client = Client::new_remote(id, info, source_node.clone());
            Some(RelayEvent::ClientIntro {
                client,
                node_id: source_node,
            })
        }
        ProtoEvent::ClientDown(cd) => {
            let id = parse_client_id(&cd.client_id)?;
            Some(RelayEvent::ClientDown { client_id: id })
        }
        ProtoEvent::NickChange(nc) => {
            let id = parse_client_id(&nc.client_id)?;
            Some(RelayEvent::NickChange {
                client_id: id,
                new_nick: nc.new_nick,
            })
        }
        ProtoEvent::Join(j) => {
            let id = parse_client_id(&j.client_id)?;
            Some(RelayEvent::Join {
                client_id: id,
                channel: j.channel,
            })
        }
        ProtoEvent::Part(p) => {
            let id = parse_client_id(&p.client_id)?;
            let reason = if p.reason.is_empty() {
                None
            } else {
                Some(p.reason)
            };
            Some(RelayEvent::Part {
                client_id: id,
                channel: p.channel,
                reason,
            })
        }
        ProtoEvent::Quit(q) => {
            let id = parse_client_id(&q.client_id)?;
            let reason = if q.reason.is_empty() {
                None
            } else {
                Some(q.reason)
            };
            Some(RelayEvent::Quit {
                client_id: id,
                reason,
            })
        }
        ProtoEvent::Privmsg(p) => {
            let id = parse_client_id(&p.client_id)?;
            let target = match p.target? {
                PrivmsgTarget::TargetChannel(ch) => ch,
                PrivmsgTarget::TargetClientId(cid) => cid,
            };
            Some(RelayEvent::Privmsg {
                client_id: id,
                target,
                text: p.text,
            })
        }
        ProtoEvent::Notice(n) => {
            let id = parse_client_id(&n.client_id)?;
            let target = match n.target? {
                NoticeTarget::TargetChannel(ch) => ch,
                NoticeTarget::TargetClientId(cid) => cid,
            };
            Some(RelayEvent::Notice {
                client_id: id,
                target,
                text: n.text,
            })
        }
        ProtoEvent::Topic(t) => {
            let id = parse_client_id(&t.client_id)?;
            Some(RelayEvent::Topic {
                client_id: id,
                channel: t.channel,
                text: t.text,
            })
        }
        ProtoEvent::Mode(m) => {
            let id = parse_client_id(&m.client_id)?;
            Some(RelayEvent::Mode {
                client_id: id,
                target: m.target,
                mode_string: m.mode_string,
            })
        }
        ProtoEvent::Kick(k) => {
            let id = parse_client_id(&k.client_id)?;
            let target_id = parse_client_id(&k.target_client_id)?;
            Some(RelayEvent::Kick {
                client_id: id,
                channel: k.channel,
                target_client_id: target_id,
                reason: k.reason,
            })
        }
        ProtoEvent::NodeUp(nu) => Some(RelayEvent::NodeUp {
            node_id: NodeId(nu.node_id),
        }),
        ProtoEvent::NodeDown(nd) => Some(RelayEvent::NodeDown {
            node_id: NodeId(nd.node_id),
        }),
        ProtoEvent::CrdtDelta(cd) => Some(RelayEvent::CrdtDelta {
            crdt_id: cd.crdt_id,
            payload: cd.payload,
        }),
        ProtoEvent::AntiEntropyRequest(r) => Some(RelayEvent::AntiEntropyRequest {
            from_node: r.from_node,
            hashes: r.hashes,
        }),
        ProtoEvent::AntiEntropyResponse(r) => Some(RelayEvent::AntiEntropyResponse {
            from_node: r.from_node,
            blobs: r.blobs,
        }),
        ProtoEvent::StateSnapshot(snap) => {
            let clients = snap
                .clients
                .into_iter()
                .map(|c| {
                    let info = ClientInfo {
                        nick: c.nick,
                        username: c.user,
                        realname: String::new(),
                        hostname: c.host,
                        registered: true,
                        identified: false,
                        account: None,
                        modes: c.modes,
                        away: if c.away.is_empty() {
                            None
                        } else {
                            Some(c.away)
                        },
                        caps: 0,
                    };
                    SnapshotClient {
                        client_id: ClientId(c.client_id),
                        info,
                        node_id: source_node.clone(),
                    }
                })
                .collect();

            let channels = snap
                .channels
                .into_iter()
                .map(|ch| {
                    let topic = if ch.topic_text.is_empty() {
                        None
                    } else {
                        Some((ch.topic_text, ch.topic_setter, ch.topic_ts))
                    };
                    let modes = ChannelModes {
                        invite_only: (ch.mode_flags & (1 << 0)) != 0,
                        topic_locked: (ch.mode_flags & (1 << 1)) != 0,
                        no_external: (ch.mode_flags & (1 << 2)) != 0,
                        moderated: (ch.mode_flags & (1 << 3)) != 0,
                        secret: (ch.mode_flags & (1 << 4)) != 0,
                        key: if ch.mode_key.is_empty() {
                            None
                        } else {
                            Some(ch.mode_key)
                        },
                        limit: if ch.mode_limit == 0 {
                            None
                        } else {
                            Some(ch.mode_limit as usize)
                        },
                    };
                    SnapshotChannel {
                        channel_id: ch.channel_id,
                        name: ch.name,
                        topic,
                        modes,
                        created_at: ch.created_at,
                    }
                })
                .collect();

            let memberships = snap
                .memberships
                .into_iter()
                .map(|m| SnapshotMembership {
                    client_id: ClientId(m.client_id),
                    channel_id: m.channel_id,
                    mode: match m.mode {
                        1 => MemberMode::Voice,
                        2 => MemberMode::Op,
                        _ => MemberMode::Normal,
                    },
                })
                .collect();

            Some(RelayEvent::StateSnapshot {
                target_node_id: NodeId(snap.target_node_id),
                clients,
                channels,
                memberships,
            })
        }
    }
}

/// Parse a `ClientId` from its string representation (e.g. `"42"`).
fn parse_client_id(s: &str) -> Option<ClientId> {
    s.parse::<u64>().ok().map(ClientId)
}
