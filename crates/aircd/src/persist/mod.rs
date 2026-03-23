//! Phase B — CRDT-backed persistent state with SQLite write-through.
//!
//! # Design
//!
//! Every piece of state that must survive restarts lives here:
//!
//! - **Ban lists** per channel: `Orswot<BanMask, NodeId>` — add-biased CRDT.
//! - **Nick registrations**: LWW map over `Identity` records.
//! - **Channel registrations**: LWW map over `RegisteredChannel` records.
//! - **Friend lists** per nick: `Orswot<friend_nick, NodeId>` — add-biased CRDT.
//! - **Silence lists** per nick: `Orswot<target_nick, NodeId>` — add-biased CRDT.
//!   Silence reasons are stored as a companion LWW map (reasons are cosmetic,
//!   not security-relevant, so LWW is correct).
//!
//! All CRDTs live in memory. Every mutation also writes through to a local
//! SQLite file (one file per node — never shared). On startup the full state
//! is loaded from SQLite into memory. On anti-entropy (Phase C) nodes exchange
//! CRDT blobs and merge them.
//!
//! # Serialisation
//!
//! Orswot CRDTs are serialised as a single blob per key using bincode.
//! Nick and channel registration entries are stored individually (one row per
//! nick / channel), with a logical clock column for LWW merge.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crdts::{CmRDT, CvRDT, Orswot};
use serde::{Deserialize, Serialize};
use sqlx::pool::PoolOptions;
use sqlx::{Pool, Row, Sqlite};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, info, warn};

use crate::util::glob_match;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Actor identifier — the node ID. A stable string assigned to each aircd
/// instance (defaults to the server hostname).
pub type NodeId = String;

/// A ban mask, e.g. `"*!*@192.168.1.*"`. Always stored lowercase.
pub type BanMask = String;

// ---------------------------------------------------------------------------
// Write-task types
// ---------------------------------------------------------------------------

/// Capacity of the bounded write-op channel.
/// Provides backpressure if SQLite falls behind; 4096 entries is ~32–128 KB
/// of metadata at typical record sizes.
const WRITE_CHANNEL_CAPACITY: usize = 4096;

/// How often (in seconds) the background compaction task purges CRDT
/// tombstones by rebuilding each Orswot with only its live members.
#[allow(dead_code)]
const CRDT_COMPACTION_INTERVAL_SECS: u64 = 3600;

/// All SQLite write operations, serialised through a single background task.
///
/// Every variant carries already-serialised blobs or plain field values so
/// that bincode work is done by the mutating task (on the Tokio thread pool)
/// and the write task only executes SQL.
enum WriteOp {
    // Upserts
    BanList {
        channel_lower: String,
        blob: Vec<u8>,
    },
    Nick {
        nick_lower: String,
        data_blob: Vec<u8>,
        clock: i64,
        node_id: String,
    },
    Channel {
        channel_lower: String,
        data_blob: Vec<u8>,
        clock: i64,
        node_id: String,
    },
    FriendList {
        nick_lower: String,
        blob: Vec<u8>,
    },
    SilenceSet {
        nick_lower: String,
        blob: Vec<u8>,
    },
    SilenceReason {
        owner_target_key: String,
        reason: Option<String>,
        clock: i64,
        node_id: String,
    },
    // Deletes
    DeleteNick {
        nick_lower: String,
    },
    DeleteChannel {
        channel_lower: String,
    },
    DeleteSilenceReason {
        owner_target_key: String,
    },
}

/// Background task that drains the `WriteOp` channel and flushes writes to
/// SQLite.  Consecutive ops are grouped into a single transaction for
/// throughput; the loop never holds a transaction open across an `await`
/// waiting for *new* ops.
async fn run_write_task(db: Pool<Sqlite>, mut rx: mpsc::Receiver<WriteOp>) {
    use sqlx::Connection;

    // Minimum batch size before we open a transaction.  A batch of 1 is fine
    // (just adds one extra BEGIN/COMMIT per single write), but avoids the
    // overhead of 1-transaction-per-write under light load.
    while let Some(first) = rx.recv().await {
        // Collect as many ops as are already queued without blocking.
        let mut batch = Vec::with_capacity(64);
        batch.push(first);
        while let Ok(op) = rx.try_recv() {
            batch.push(op);
            if batch.len() >= 1024 {
                break; // cap batch to avoid unbounded latency (DB-3: raised from 256→1024
                // to reduce transaction overhead under burst load)
            }
        }

        let now = now_unix() as i64;
        let mut conn = match db.acquire().await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "write_task: failed to acquire db connection");
                continue;
            }
        };
        let tx_result = conn.begin().await;
        let mut tx = match tx_result {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "write_task: failed to begin transaction");
                continue;
            }
        };

        for op in batch {
            let result: Result<_, sqlx::Error> = match op {
                WriteOp::BanList { channel_lower, blob } => {
                    sqlx::query(
                        "INSERT INTO ban_lists (channel, crdt_blob, updated_at) \
                         VALUES (?, ?, ?) \
                         ON CONFLICT(channel) DO UPDATE SET crdt_blob = excluded.crdt_blob, \
                                                            updated_at = excluded.updated_at",
                    )
                    .bind(&channel_lower)
                    .bind(blob)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::Nick { nick_lower, data_blob, clock, node_id } => {
                    sqlx::query(
                        "INSERT INTO nick_registrations (nick_lower, data_blob, clock, node_id, updated_at) \
                         VALUES (?, ?, ?, ?, ?) \
                         ON CONFLICT(nick_lower) DO UPDATE SET data_blob = excluded.data_blob, \
                                                               clock = excluded.clock, \
                                                               node_id = excluded.node_id, \
                                                               updated_at = excluded.updated_at",
                    )
                    .bind(&nick_lower)
                    .bind(data_blob)
                    .bind(clock)
                    .bind(&node_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::Channel { channel_lower, data_blob, clock, node_id } => {
                    sqlx::query(
                        "INSERT INTO channel_registrations (channel_lower, data_blob, clock, node_id, updated_at) \
                         VALUES (?, ?, ?, ?, ?) \
                         ON CONFLICT(channel_lower) DO UPDATE SET data_blob = excluded.data_blob, \
                                                                  clock = excluded.clock, \
                                                                  node_id = excluded.node_id, \
                                                                  updated_at = excluded.updated_at",
                    )
                    .bind(&channel_lower)
                    .bind(data_blob)
                    .bind(clock)
                    .bind(&node_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::FriendList { nick_lower, blob } => {
                    sqlx::query(
                        "INSERT INTO friend_lists (nick_lower, crdt_blob, updated_at) \
                         VALUES (?, ?, ?) \
                         ON CONFLICT(nick_lower) DO UPDATE SET crdt_blob = excluded.crdt_blob, \
                                                               updated_at = excluded.updated_at",
                    )
                    .bind(&nick_lower)
                    .bind(blob)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::SilenceSet { nick_lower, blob } => {
                    sqlx::query(
                        "INSERT INTO silence_sets (nick_lower, crdt_blob, updated_at) \
                         VALUES (?, ?, ?) \
                         ON CONFLICT(nick_lower) DO UPDATE SET crdt_blob = excluded.crdt_blob, \
                                                               updated_at = excluded.updated_at",
                    )
                    .bind(&nick_lower)
                    .bind(blob)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::SilenceReason { owner_target_key, reason, clock, node_id } => {
                    sqlx::query(
                        "INSERT INTO silence_reasons (owner_target_key, reason, clock, node_id, updated_at) \
                         VALUES (?, ?, ?, ?, ?) \
                         ON CONFLICT(owner_target_key) DO UPDATE SET reason = excluded.reason, \
                                                                      clock = excluded.clock, \
                                                                      node_id = excluded.node_id, \
                                                                      updated_at = excluded.updated_at",
                    )
                    .bind(&owner_target_key)
                    .bind(reason)
                    .bind(clock)
                    .bind(&node_id)
                    .bind(now)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::DeleteNick { nick_lower } => {
                    sqlx::query("DELETE FROM nick_registrations WHERE nick_lower = ?")
                        .bind(&nick_lower)
                        .execute(&mut *tx)
                        .await
                        .map(|_| ())
                }
                WriteOp::DeleteChannel { channel_lower } => {
                    sqlx::query(
                        "DELETE FROM channel_registrations WHERE channel_lower = ?",
                    )
                    .bind(&channel_lower)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
                WriteOp::DeleteSilenceReason { owner_target_key } => {
                    sqlx::query(
                        "DELETE FROM silence_reasons WHERE owner_target_key = ?",
                    )
                    .bind(&owner_target_key)
                    .execute(&mut *tx)
                    .await
                    .map(|_| ())
                }
            };
            if let Err(e) = result {
                warn!(error = %e, "write_task: SQL error (continuing batch)");
            }
        }

        if let Err(e) = tx.commit().await {
            warn!(error = %e, "write_task: failed to commit transaction");
        }
    }
    debug!("write_task: channel closed, exiting");
}

// ---------------------------------------------------------------------------
// LWW entry wrappers (nick and channel registrations)
// ---------------------------------------------------------------------------

/// LWW-register wrapper around a value with a logical clock and actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LwwEntry<T> {
    pub value: T,
    /// Logical clock — monotonically increasing per actor.
    pub clock: u64,
    /// The actor (node) that wrote this value.
    pub node_id: NodeId,
}

// ---------------------------------------------------------------------------
// PersistentState
// ---------------------------------------------------------------------------

/// All CRDT-backed persistent state for one aircd node.
///
/// Cheaply cloneable (`Arc` inside). All public methods are `async` and
/// acquire the minimum necessary lock scope.
#[derive(Clone)]
pub struct PersistentState {
    inner: Arc<Inner>,
}

struct Inner {
    /// Per-channel ban lists. Key is lowercase channel name.
    ban_lists: RwLock<HashMap<String, Orswot<BanMask, NodeId>>>,
    /// Nick registrations (LWW). Key is lowercase nick.
    nick_registrations: RwLock<HashMap<String, LwwEntry<NickRecord>>>,
    /// Channel registrations (LWW). Key is lowercase channel name.
    channel_registrations: RwLock<HashMap<String, LwwEntry<ChannelRecord>>>,
    /// Per-nick friend lists. Key is lowercase nick (owner).
    friend_lists: RwLock<HashMap<String, Orswot<String, NodeId>>>,
    /// Per-nick silence sets. Key is lowercase nick (owner).
    silence_sets: RwLock<HashMap<String, Orswot<String, NodeId>>>,
    /// Silence reasons: owner_lower → target_lower → reason (LWW).
    silence_reasons: RwLock<HashMap<String, LwwEntry<Option<String>>>>,
    /// This node's actor identifier (used for CRDT operations).
    node_id: NodeId,
    /// Gossip channel: after each CRDT mutation, send `(crdt_id, blob)` here
    /// so the server task can forward it to remote nodes via the relay.
    /// `None` until `set_gossip_tx()` is called (i.e. before relay is wired).
    /// Bounded — gossip is best-effort; blobs dropped when full are recovered
    /// by anti-entropy on reconnect.
    gossip_tx: std::sync::OnceLock<mpsc::Sender<(String, Vec<u8>)>>,
    /// Bounded channel to the background SQLite write task.
    /// Mutations enqueue a `WriteOp` here and return immediately.
    write_tx: mpsc::Sender<WriteOp>,
}

// ---------------------------------------------------------------------------
// Stored record types
// ---------------------------------------------------------------------------

/// Minimal nick registration record stored in PersistentState.
/// Full `Identity` lives in NickServState; this is the CRDT-managed copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NickRecord {
    pub nick: String,
    /// StoredKey for SCRAM-SHA-256, hex-encoded.
    pub scram_stored_key: Option<String>,
    /// ServerKey for SCRAM-SHA-256, hex-encoded.
    pub scram_server_key: Option<String>,
    /// Random 16-byte PBKDF2 salt, hex-encoded.
    pub scram_salt: Option<String>,
    /// PBKDF2 iteration count used during registration.
    pub scram_iterations: Option<u32>,
    /// bcrypt hash of the password (for PLAIN auth).
    pub bcrypt_hash: Option<String>,
    pub pubkey_hex: Option<String>,
    pub registered_at: u64,
    pub reputation: i64,
    pub capabilities: Vec<String>,
}

/// Minimal channel registration record stored in PersistentState.
/// Full `RegisteredChannel` lives in ChanServState; this is the CRDT copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelRecord {
    pub name: String,
    pub founder: String,
    pub min_reputation: i64,
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Open (or create) the SQLite database at `db_path`, run migrations, load
    /// all state into memory, and return a ready `PersistentState`.
    pub async fn open(db_path: &Path, node_id: impl Into<NodeId>) -> Result<Self, sqlx::Error> {
        // Ensure parent directory exists.
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        let url = format!(
            "sqlite://{}?mode=rwc",
            db_path.to_str().expect("db path is valid UTF-8")
        );
        // SQLite does not support concurrent writes: cap the pool at 1 connection
        // to avoid SQLITE_BUSY errors under concurrent write load.
        let pool = PoolOptions::<Sqlite>::new()
            .max_connections(1)
            .connect(&url)
            .await?;

        // Enable WAL mode: allows concurrent reads while a write is in progress,
        // which is best practice for SQLite with a single writer pool.
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await?;

        // Run embedded migrations.
        run_migrations(&pool).await?;

        let node_id: NodeId = node_id.into();

        let mut ban_lists: HashMap<String, Orswot<BanMask, NodeId>> = HashMap::new();
        let mut nick_registrations: HashMap<String, LwwEntry<NickRecord>> = HashMap::new();
        let mut channel_registrations: HashMap<String, LwwEntry<ChannelRecord>> = HashMap::new();
        let mut friend_lists: HashMap<String, Orswot<String, NodeId>> = HashMap::new();
        let mut silence_sets: HashMap<String, Orswot<String, NodeId>> = HashMap::new();
        let mut silence_reasons: HashMap<String, LwwEntry<Option<String>>> = HashMap::new();

        // Load ban lists.
        let rows = sqlx::query("SELECT channel, crdt_blob FROM ban_lists")
            .fetch_all(&pool)
            .await?;
        for row in &rows {
            let channel: String = row.get("channel");
            let blob: Vec<u8> = row.get("crdt_blob");
            match bincode::serde::decode_from_slice::<Orswot<BanMask, NodeId>, _>(
                &blob,
                bincode::config::standard(),
            ) {
                Ok((orswot, _)) => {
                    ban_lists.insert(channel, orswot);
                }
                Err(e) => {
                    warn!(channel = %channel, error = %e, "PersistentState: failed to decode ban list, skipping");
                }
            }
        }
        info!(count = ban_lists.len(), "PersistentState: loaded ban lists");

        // Load nick registrations.
        let rows =
            sqlx::query("SELECT nick_lower, data_blob, clock, node_id FROM nick_registrations")
                .fetch_all(&pool)
                .await?;
        for row in &rows {
            let nick_lower: String = row.get("nick_lower");
            let blob: Vec<u8> = row.get("data_blob");
            let clock: i64 = row.get("clock");
            let nid: String = row.get("node_id");
            match bincode::serde::decode_from_slice::<NickRecord, _>(
                &blob,
                bincode::config::standard(),
            ) {
                Ok((record, _)) => {
                    nick_registrations.insert(
                        nick_lower,
                        LwwEntry {
                            value: record,
                            clock: clock as u64,
                            node_id: nid,
                        },
                    );
                }
                Err(e) => {
                    warn!(error = %e, "PersistentState: failed to decode nick record, skipping");
                }
            }
        }
        info!(
            count = nick_registrations.len(),
            "PersistentState: loaded nick registrations"
        );

        // Load channel registrations.
        let rows = sqlx::query(
            "SELECT channel_lower, data_blob, clock, node_id FROM channel_registrations",
        )
        .fetch_all(&pool)
        .await?;
        for row in &rows {
            let channel_lower: String = row.get("channel_lower");
            let blob: Vec<u8> = row.get("data_blob");
            let clock: i64 = row.get("clock");
            let nid: String = row.get("node_id");
            match bincode::serde::decode_from_slice::<ChannelRecord, _>(
                &blob,
                bincode::config::standard(),
            ) {
                Ok((record, _)) => {
                    channel_registrations.insert(
                        channel_lower,
                        LwwEntry {
                            value: record,
                            clock: clock as u64,
                            node_id: nid,
                        },
                    );
                }
                Err(e) => {
                    warn!(error = %e, "PersistentState: failed to decode channel record, skipping");
                }
            }
        }
        info!(
            count = channel_registrations.len(),
            "PersistentState: loaded channel registrations"
        );

        // Load friend lists.
        let rows = sqlx::query("SELECT nick_lower, crdt_blob FROM friend_lists")
            .fetch_all(&pool)
            .await?;
        for row in &rows {
            let nick_lower: String = row.get("nick_lower");
            let blob: Vec<u8> = row.get("crdt_blob");
            match bincode::serde::decode_from_slice::<Orswot<String, NodeId>, _>(
                &blob,
                bincode::config::standard(),
            ) {
                Ok((orswot, _)) => {
                    friend_lists.insert(nick_lower, orswot);
                }
                Err(e) => {
                    warn!(nick = %nick_lower, error = %e, "PersistentState: failed to decode friend list, skipping");
                }
            }
        }
        info!(
            count = friend_lists.len(),
            "PersistentState: loaded friend lists"
        );

        // Load silence sets.
        let rows = sqlx::query("SELECT nick_lower, crdt_blob FROM silence_sets")
            .fetch_all(&pool)
            .await?;
        for row in &rows {
            let nick_lower: String = row.get("nick_lower");
            let blob: Vec<u8> = row.get("crdt_blob");
            match bincode::serde::decode_from_slice::<Orswot<String, NodeId>, _>(
                &blob,
                bincode::config::standard(),
            ) {
                Ok((orswot, _)) => {
                    silence_sets.insert(nick_lower, orswot);
                }
                Err(e) => {
                    warn!(nick = %nick_lower, error = %e, "PersistentState: failed to decode silence set, skipping");
                }
            }
        }
        info!(
            count = silence_sets.len(),
            "PersistentState: loaded silence sets"
        );

        // Load silence reasons.
        let rows =
            sqlx::query("SELECT owner_target_key, reason, clock, node_id FROM silence_reasons")
                .fetch_all(&pool)
                .await?;
        for row in &rows {
            let key: String = row.get("owner_target_key");
            let reason: Option<String> = row.get("reason");
            let clock: i64 = row.get("clock");
            let nid: String = row.get("node_id");
            silence_reasons.insert(
                key,
                LwwEntry {
                    value: reason,
                    clock: clock as u64,
                    node_id: nid,
                },
            );
        }
        info!(
            count = silence_reasons.len(),
            "PersistentState: loaded silence reasons"
        );

        Ok(Self {
            inner: Arc::new(Inner {
                ban_lists: RwLock::new(ban_lists),
                nick_registrations: RwLock::new(nick_registrations),
                channel_registrations: RwLock::new(channel_registrations),
                friend_lists: RwLock::new(friend_lists),
                silence_sets: RwLock::new(silence_sets),
                silence_reasons: RwLock::new(silence_reasons),
                node_id,
                gossip_tx: std::sync::OnceLock::new(),
                write_tx: {
                    let (tx, rx) = mpsc::channel(WRITE_CHANNEL_CAPACITY);
                    tokio::spawn(run_write_task(pool, rx));
                    tx
                },
            }),
        })
    }

    /// Spawn the background CRDT tombstone compaction task.
    ///
    /// Must be called once after `open()`. The task runs forever (until the
    /// Tokio runtime shuts down), waking every `CRDT_COMPACTION_INTERVAL_SECS`
    /// seconds to compact tombstones in all Orswot CRDTs.
    #[allow(dead_code)]
    pub fn spawn_compaction_task(&self) {
        let ps = self.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(std::time::Duration::from_secs(
                CRDT_COMPACTION_INTERVAL_SECS,
            ));
            // The first tick fires immediately; skip it so compaction does not
            // run at startup (the data was just loaded fresh from SQLite).
            interval.tick().await;
            loop {
                interval.tick().await;
                ps.compact_crdt_tombstones().await;
            }
        });
    }

    /// Node identifier for this instance.
    #[allow(dead_code)]
    pub fn node_id(&self) -> &str {
        &self.inner.node_id
    }

    /// Wire up the gossip channel. Must be called once after startup, before
    /// the relay is fully running. After this, every CRDT mutation will send
    /// `(crdt_id, blob)` to `tx` for the server task to forward via the relay.
    pub fn set_gossip_tx(&self, tx: mpsc::Sender<(String, Vec<u8>)>) {
        let _ = self.inner.gossip_tx.set(tx);
    }

    /// Internal helper: send a CRDT blob to the gossip channel (if wired).
    /// Uses `try_send` — if the channel is full the blob is dropped.  This is
    /// intentional: gossip is best-effort, and nodes recover stale state via
    /// anti-entropy on reconnect.  Drops are logged at WARN level.
    async fn gossip(&self, crdt_id: &str, blob: Vec<u8>) {
        if let Some(tx) = self.inner.gossip_tx.get()
            && let Err(e) = tx.try_send((crdt_id.to_string(), blob))
        {
            warn!(crdt_id = %crdt_id, error = %e, "gossip: channel full, dropping CRDT delta (anti-entropy will recover)");
        }
    }

    /// Internal helper: enqueue a write operation for the background write task.
    /// Returns immediately — the caller does not wait for the SQLite write.
    ///
    /// Uses `try_send` first for the common (non-full) case.  If the channel
    /// is full (SQLite is lagging) we fall back to spawning a task that blocks
    /// until space is available, so the write is never silently dropped.
    fn enqueue_write(&self, op: WriteOp) {
        match self.inner.write_tx.try_send(op) {
            Ok(_) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(op)) => {
                // Channel is full — SQLite is lagging.  Spawn a task to wait
                // for space rather than silently dropping the write.
                let tx = self.inner.write_tx.clone();
                warn!("PersistentState: write queue full, spawning backpressure task");
                tokio::spawn(async move {
                    if tx.send(op).await.is_err() {
                        // write task has exited — server is shutting down
                    }
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                // write task has exited — server is shutting down, ignore
            }
        }
    }

    /// Compact CRDT tombstones in all Orswot-backed collections.
    ///
    /// An Orswot accumulates causal history for removed entries indefinitely.
    /// This method rebuilds every Orswot from scratch, re-adding only the
    /// currently live members into a fresh `Orswot::new()`.  All tombstones
    /// (causal history for entries that no longer exist) are discarded.
    ///
    /// This is safe for a single-node deployment and for a gossip cluster where
    /// compaction is run on all nodes simultaneously: after compaction the CRDT
    /// still converges correctly because every node holds the same live set.
    ///
    /// Compacted CRDTs are written through to SQLite so the trimmed state
    /// survives restarts.
    pub async fn compact_crdt_tombstones(&self) {
        let node_id = self.inner.node_id.clone();

        // --- ban_lists ---
        {
            let mut ban_lists = self.inner.ban_lists.write().await;
            let mut compacted_ban: Vec<(String, Orswot<BanMask, NodeId>)> =
                Vec::with_capacity(ban_lists.len());
            for (key, old) in ban_lists.iter() {
                let members: Vec<BanMask> = old.read().val.into_iter().collect();
                let mut fresh: Orswot<BanMask, NodeId> = Orswot::new();
                for member in members {
                    let add_ctx = fresh.read_ctx().derive_add_ctx(node_id.clone());
                    let op = fresh.add(member, add_ctx);
                    fresh.apply(op);
                }
                compacted_ban.push((key.clone(), fresh));
            }
            let mut count = 0usize;
            for (key, fresh) in compacted_ban {
                if let Some(slot) = ban_lists.get_mut(&key) {
                    *slot = fresh;
                    count += 1;
                }
            }
            // Persist the compacted blobs (encode under the write lock).
            let writes: Vec<(String, Option<Vec<u8>>)> = ban_lists
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        bincode::serde::encode_to_vec(v, bincode::config::standard()).ok(),
                    )
                })
                .collect();
            drop(ban_lists);
            for (key, blob) in writes {
                if let Some(blob) = blob {
                    self.persist_ban_list_bytes(&key, blob);
                }
            }
            if count > 0 {
                info!(count, "compact_crdt_tombstones: compacted ban_lists");
            }
        }

        // --- friend_lists ---
        {
            let mut friend_lists = self.inner.friend_lists.write().await;
            let mut compacted: Vec<(String, Orswot<String, NodeId>)> =
                Vec::with_capacity(friend_lists.len());
            for (key, old) in friend_lists.iter() {
                let members: Vec<String> = old.read().val.into_iter().collect();
                let mut fresh: Orswot<String, NodeId> = Orswot::new();
                for member in members {
                    let add_ctx = fresh.read_ctx().derive_add_ctx(node_id.clone());
                    let op = fresh.add(member, add_ctx);
                    fresh.apply(op);
                }
                compacted.push((key.clone(), fresh));
            }
            let mut count = 0usize;
            for (key, fresh) in compacted {
                if let Some(slot) = friend_lists.get_mut(&key) {
                    *slot = fresh;
                    count += 1;
                }
            }
            let writes: Vec<(String, Option<Vec<u8>>)> = friend_lists
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        bincode::serde::encode_to_vec(v, bincode::config::standard()).ok(),
                    )
                })
                .collect();
            drop(friend_lists);
            for (key, blob) in writes {
                if let Some(blob) = blob {
                    self.persist_friend_list_bytes(&key, blob);
                }
            }
            if count > 0 {
                info!(count, "compact_crdt_tombstones: compacted friend_lists");
            }
        }

        // --- silence_sets ---
        {
            let mut silence_sets = self.inner.silence_sets.write().await;
            let mut compacted: Vec<(String, Orswot<String, NodeId>)> =
                Vec::with_capacity(silence_sets.len());
            for (key, old) in silence_sets.iter() {
                let members: Vec<String> = old.read().val.into_iter().collect();
                let mut fresh: Orswot<String, NodeId> = Orswot::new();
                for member in members {
                    let add_ctx = fresh.read_ctx().derive_add_ctx(node_id.clone());
                    let op = fresh.add(member, add_ctx);
                    fresh.apply(op);
                }
                compacted.push((key.clone(), fresh));
            }
            let mut count = 0usize;
            for (key, fresh) in compacted {
                if let Some(slot) = silence_sets.get_mut(&key) {
                    *slot = fresh;
                    count += 1;
                }
            }
            let writes: Vec<(String, Option<Vec<u8>>)> = silence_sets
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        bincode::serde::encode_to_vec(v, bincode::config::standard()).ok(),
                    )
                })
                .collect();
            drop(silence_sets);
            for (key, blob) in writes {
                if let Some(blob) = blob {
                    self.persist_silence_set_bytes(&key, blob);
                }
            }
            if count > 0 {
                info!(count, "compact_crdt_tombstones: compacted silence_sets");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ban list operations
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Add a ban mask to a channel's ban list. Returns `true` if the mask was
    /// not already present. Writes through to SQLite.
    pub async fn add_ban(&self, channel: &str, mask: BanMask) -> bool {
        let key = channel.to_ascii_lowercase();
        let mask_lower = mask.to_ascii_lowercase();

        let mut ban_lists = self.inner.ban_lists.write().await;
        let orswot = ban_lists.entry(key.clone()).or_insert_with(Orswot::new);

        let read_ctx = orswot.contains(&mask_lower);
        if read_ctx.val {
            return false;
        }

        let add_ctx = orswot.read_ctx().derive_add_ctx(self.inner.node_id.clone());
        let op = orswot.add(mask_lower, add_ctx);
        orswot.apply(op);

        // Encode to bytes under the write lock to avoid a full orswot clone.
        let blob = bincode::serde::encode_to_vec(&*orswot, bincode::config::standard()).ok();
        drop(ban_lists);

        if let Some(blob) = blob {
            self.persist_ban_list_bytes(&key, blob.clone());
            self.gossip(&format!("ban:{key}"), blob).await;
        }
        true
    }

    /// Remove a ban mask from a channel's ban list. Returns `true` if removed.
    /// Writes through to SQLite.
    pub async fn remove_ban(&self, channel: &str, mask: &str) -> bool {
        let key = channel.to_ascii_lowercase();
        let mask_lower = mask.to_ascii_lowercase();

        let mut ban_lists = self.inner.ban_lists.write().await;
        let Some(orswot) = ban_lists.get_mut(&key) else {
            return false;
        };

        let read_ctx = orswot.contains(&mask_lower);
        if !read_ctx.val {
            return false;
        }

        let rm_ctx = read_ctx.derive_rm_ctx();
        let op = orswot.rm(mask_lower, rm_ctx);
        orswot.apply(op);

        // Encode to bytes under the write lock to avoid a full orswot clone.
        let blob = bincode::serde::encode_to_vec(&*orswot, bincode::config::standard()).ok();
        drop(ban_lists);

        if let Some(blob) = blob {
            self.persist_ban_list_bytes(&key, blob.clone());
            self.gossip(&format!("ban:{key}"), blob).await;
        }
        true
    }

    /// Return all current ban masks for a channel (snapshot).
    pub async fn get_bans(&self, channel: &str) -> Vec<BanMask> {
        let key = channel.to_ascii_lowercase();
        let ban_lists = self.inner.ban_lists.read().await;
        match ban_lists.get(&key) {
            Some(orswot) => orswot.read().val.into_iter().collect(),
            None => vec![],
        }
    }

    /// Check whether a nick (or `nick!user@host`) matches any ban mask in a channel.
    ///
    /// The check is performed in-place under a single read lock without
    /// collecting the ban list into an intermediate `Vec`.
    pub async fn is_banned(&self, channel: &str, nick: &str, userhost: Option<&str>) -> bool {
        let key = channel.to_ascii_lowercase();
        let nick_lower = nick.to_ascii_lowercase();
        let full = userhost.map(|uh| format!("{}!{}", nick_lower, uh.to_ascii_lowercase()));

        let ban_lists = self.inner.ban_lists.read().await;
        let Some(orswot) = ban_lists.get(&key) else {
            return false;
        };
        orswot.read().val.iter().any(|mask| {
            glob_match(mask, &nick_lower) || full.as_deref().is_some_and(|f| glob_match(mask, f))
        })
    }

    /// Merge an incoming ban-list CRDT blob from a peer node (Phase C anti-entropy).
    pub async fn merge_ban_list(&self, channel: &str, blob: &[u8]) {
        let key = channel.to_ascii_lowercase();
        match bincode::serde::decode_from_slice::<Orswot<BanMask, NodeId>, _>(
            blob,
            bincode::config::standard(),
        ) {
            Ok((remote, _)) => {
                let mut ban_lists = self.inner.ban_lists.write().await;
                let local = ban_lists.entry(key.clone()).or_insert_with(Orswot::new);
                local.merge(remote);
                // Encode under the write lock to avoid cloning the CRDT.
                let blob = bincode::serde::encode_to_vec(&*local, bincode::config::standard()).ok();
                drop(ban_lists);
                if let Some(blob) = blob {
                    self.persist_ban_list_bytes(&key, blob);
                }
            }
            Err(e) => {
                warn!(channel = %channel, error = %e, "merge_ban_list: failed to decode blob");
            }
        }
    }

    /// Serialise the ban list for a channel to a blob (for anti-entropy export).
    pub async fn export_ban_list(&self, channel: &str) -> Option<Vec<u8>> {
        let key = channel.to_ascii_lowercase();
        let ban_lists = self.inner.ban_lists.read().await;
        let orswot = ban_lists.get(&key)?;
        bincode::serde::encode_to_vec(orswot, bincode::config::standard()).ok()
    }

    #[allow(dead_code)]
    fn persist_ban_list(&self, channel_lower: &str, orswot: &Orswot<BanMask, NodeId>) {
        match bincode::serde::encode_to_vec(orswot, bincode::config::standard()) {
            Ok(blob) => self.enqueue_write(WriteOp::BanList {
                channel_lower: channel_lower.to_string(),
                blob,
            }),
            Err(e) => {
                warn!(channel = %channel_lower, error = %e, "PersistentState: failed to encode ban list");
            }
        }
    }

    /// Persist a ban list from already-encoded bytes (avoids re-encoding).
    fn persist_ban_list_bytes(&self, channel_lower: &str, blob: Vec<u8>) {
        self.enqueue_write(WriteOp::BanList {
            channel_lower: channel_lower.to_string(),
            blob,
        });
    }
}

// ---------------------------------------------------------------------------
// Nick registration operations
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Upsert a nick registration (LWW: highest clock wins on conflict).
    /// Writes through to SQLite.
    pub async fn upsert_nick(&self, record: NickRecord) {
        let key = record.nick.to_ascii_lowercase();
        let now = now_unix();

        let entry = LwwEntry {
            clock: now,
            node_id: self.inner.node_id.clone(),
            value: record,
        };

        let mut regs = self.inner.nick_registrations.write().await;
        regs.insert(key.clone(), entry.clone());
        drop(regs);

        self.persist_nick(&key, &entry);
        // Gossip: encode the full LwwEntry<NickRecord> blob.
        if let Ok(blob) = bincode::serde::encode_to_vec(&entry, bincode::config::standard()) {
            self.gossip(&format!("nick:{key}"), blob).await;
        }
    }

    /// Look up a nick registration by lowercase nick.
    #[allow(dead_code)]
    pub async fn get_nick(&self, nick: &str) -> Option<NickRecord> {
        let key = nick.to_ascii_lowercase();
        self.inner
            .nick_registrations
            .read()
            .await
            .get(&key)
            .map(|e| e.value.clone())
    }

    /// Remove a nick registration. Writes through to SQLite.
    #[allow(dead_code)]
    pub async fn remove_nick(&self, nick: &str) {
        let key = nick.to_ascii_lowercase();
        self.inner.nick_registrations.write().await.remove(&key);
        self.enqueue_write(WriteOp::DeleteNick { nick_lower: key });
    }

    fn persist_nick(&self, nick_lower: &str, entry: &LwwEntry<NickRecord>) {
        match bincode::serde::encode_to_vec(&entry.value, bincode::config::standard()) {
            Ok(data_blob) => self.enqueue_write(WriteOp::Nick {
                nick_lower: nick_lower.to_string(),
                data_blob,
                clock: entry.clock as i64,
                node_id: entry.node_id.clone(),
            }),
            Err(e) => {
                warn!(nick = %nick_lower, error = %e, "PersistentState: failed to encode nick record");
            }
        }
    }

    /// Merge a nick record from a peer (LWW: keep whichever has higher clock).
    pub async fn merge_nick(&self, nick_lower: &str, remote: LwwEntry<NickRecord>) {
        let mut regs = self.inner.nick_registrations.write().await;
        let should_update = match regs.get(nick_lower) {
            None => true,
            Some(local) => {
                remote.clock > local.clock
                    || (remote.clock == local.clock && remote.node_id > local.node_id)
            }
        };
        if should_update {
            regs.insert(nick_lower.to_string(), remote.clone());
            drop(regs);
            self.persist_nick(nick_lower, &remote);
        }
    }

    /// Iterate all nick registrations (for NickServ startup import).
    pub async fn all_nicks(&self) -> Vec<(String, NickRecord)> {
        self.inner
            .nick_registrations
            .read()
            .await
            .iter()
            .map(|(k, e)| (k.clone(), e.value.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Channel registration operations
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Upsert a channel registration (LWW). Writes through to SQLite.
    pub async fn upsert_channel(&self, record: ChannelRecord) {
        let key = record.name.to_ascii_lowercase();
        let now = now_unix();

        let entry = LwwEntry {
            clock: now,
            node_id: self.inner.node_id.clone(),
            value: record,
        };

        let mut regs = self.inner.channel_registrations.write().await;
        regs.insert(key.clone(), entry.clone());
        drop(regs);

        self.persist_channel(&key, &entry);
        if let Ok(blob) = bincode::serde::encode_to_vec(&entry, bincode::config::standard()) {
            self.gossip(&format!("channel:{key}"), blob).await;
        }
    }

    /// Look up a channel registration.
    #[allow(dead_code)]
    pub async fn get_channel(&self, channel: &str) -> Option<ChannelRecord> {
        let key = channel.to_ascii_lowercase();
        self.inner
            .channel_registrations
            .read()
            .await
            .get(&key)
            .map(|e| e.value.clone())
    }

    /// Remove a channel registration. Writes through to SQLite.
    #[allow(dead_code)]
    pub async fn remove_channel(&self, channel: &str) {
        let key = channel.to_ascii_lowercase();
        self.inner.channel_registrations.write().await.remove(&key);
        self.enqueue_write(WriteOp::DeleteChannel { channel_lower: key });
    }

    fn persist_channel(&self, channel_lower: &str, entry: &LwwEntry<ChannelRecord>) {
        match bincode::serde::encode_to_vec(&entry.value, bincode::config::standard()) {
            Ok(data_blob) => self.enqueue_write(WriteOp::Channel {
                channel_lower: channel_lower.to_string(),
                data_blob,
                clock: entry.clock as i64,
                node_id: entry.node_id.clone(),
            }),
            Err(e) => {
                warn!(channel = %channel_lower, error = %e, "PersistentState: failed to encode channel record");
            }
        }
    }

    /// Merge a channel record from a peer (LWW).
    pub async fn merge_channel(&self, channel_lower: &str, remote: LwwEntry<ChannelRecord>) {
        let mut regs = self.inner.channel_registrations.write().await;
        let should_update = match regs.get(channel_lower) {
            None => true,
            Some(local) => {
                remote.clock > local.clock
                    || (remote.clock == local.clock && remote.node_id > local.node_id)
            }
        };
        if should_update {
            regs.insert(channel_lower.to_string(), remote.clone());
            drop(regs);
            self.persist_channel(channel_lower, &remote);
        }
    }

    /// Iterate all channel registrations (for ChanServ startup import).
    pub async fn all_channels(&self) -> Vec<(String, ChannelRecord)> {
        self.inner
            .channel_registrations
            .read()
            .await
            .iter()
            .map(|(k, e)| (k.clone(), e.value.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Friend list operations
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Add a friend to a nick's friend list. Returns `true` if added.
    /// Writes through to SQLite.
    /// Add a friend to a nick's friend list. Returns `true` if added.
    /// Writes through to SQLite.
    ///
    /// For batched mutations prefer `batch_friend_ops`.
    #[allow(dead_code)]
    pub async fn add_friend(&self, nick: &str, friend: &str) -> bool {
        let key = nick.to_ascii_lowercase();
        let friend_lower = friend.to_ascii_lowercase();

        let mut lists = self.inner.friend_lists.write().await;
        let orswot = lists.entry(key.clone()).or_insert_with(Orswot::new);

        let read_ctx = orswot.contains(&friend_lower);
        if read_ctx.val {
            return false;
        }

        let add_ctx = orswot.read_ctx().derive_add_ctx(self.inner.node_id.clone());
        let op = orswot.add(friend_lower, add_ctx);
        orswot.apply(op);

        // Encode under the write lock to avoid a full orswot clone.
        let blob = bincode::serde::encode_to_vec(&*orswot, bincode::config::standard()).ok();
        drop(lists);

        if let Some(blob) = blob {
            self.persist_friend_list_bytes(&key, blob.clone());
            self.gossip(&format!("friend:{key}"), blob).await;
        }
        true
    }

    /// Remove a friend from a nick's friend list. Returns `true` if removed.
    /// Writes through to SQLite.
    ///
    /// For batched mutations prefer `batch_friend_ops`.
    #[allow(dead_code)]
    pub async fn remove_friend(&self, nick: &str, friend: &str) -> bool {
        let key = nick.to_ascii_lowercase();
        let friend_lower = friend.to_ascii_lowercase();

        let mut lists = self.inner.friend_lists.write().await;
        let Some(orswot) = lists.get_mut(&key) else {
            return false;
        };

        let read_ctx = orswot.contains(&friend_lower);
        if !read_ctx.val {
            return false;
        }

        let rm_ctx = read_ctx.derive_rm_ctx();
        let op = orswot.rm(friend_lower, rm_ctx);
        orswot.apply(op);

        // Encode under the write lock to avoid a full orswot clone.
        let blob = bincode::serde::encode_to_vec(&*orswot, bincode::config::standard()).ok();
        drop(lists);

        if let Some(blob) = blob {
            self.persist_friend_list_bytes(&key, blob.clone());
            self.gossip(&format!("friend:{key}"), blob).await;
        }
        true
    }

    /// Apply a batch of friend-list mutations for a single nick in one lock
    /// acquisition.  Returns `(added, removed)` counts.
    ///
    /// All CRDT ops are applied under a single `friend_lists` write-lock, then
    /// a single persist + gossip is issued — O(1) lock acquisitions regardless
    /// of batch size.
    pub async fn batch_friend_ops(
        &self,
        nick: &str,
        adds: &[String],
        removes: &[String],
    ) -> (usize, usize) {
        if adds.is_empty() && removes.is_empty() {
            return (0, 0);
        }
        let key = nick.to_ascii_lowercase();

        let mut lists = self.inner.friend_lists.write().await;
        let orswot = lists.entry(key.clone()).or_insert_with(Orswot::new);

        let mut added = 0usize;
        let mut removed = 0usize;

        for friend in adds {
            let friend_lower = friend.to_ascii_lowercase();
            let read_ctx = orswot.contains(&friend_lower);
            if read_ctx.val {
                continue; // already a friend
            }
            let add_ctx = orswot.read_ctx().derive_add_ctx(self.inner.node_id.clone());
            let op = orswot.add(friend_lower, add_ctx);
            orswot.apply(op);
            added += 1;
        }

        for friend in removes {
            let friend_lower = friend.to_ascii_lowercase();
            let read_ctx = orswot.contains(&friend_lower);
            if !read_ctx.val {
                continue; // not in list
            }
            let rm_ctx = read_ctx.derive_rm_ctx();
            let op = orswot.rm(friend_lower, rm_ctx);
            orswot.apply(op);
            removed += 1;
        }

        if added == 0 && removed == 0 {
            return (0, 0);
        }

        let blob_result = bincode::serde::encode_to_vec(orswot, bincode::config::standard());
        drop(lists);

        if let Ok(blob) = blob_result {
            self.persist_friend_list_bytes(&key, blob.clone());
            self.gossip(&format!("friend:{key}"), blob).await;
        }
        (added, removed)
    }

    /// Return all friends for a nick.
    pub async fn get_friends(&self, nick: &str) -> Vec<String> {
        let key = nick.to_ascii_lowercase();
        let lists = self.inner.friend_lists.read().await;
        match lists.get(&key) {
            Some(orswot) => orswot.read().val.into_iter().collect(),
            None => vec![],
        }
    }

    /// Iterate all friend lists (for NickServ startup import).
    #[allow(dead_code)]
    pub async fn all_friend_lists(&self) -> Vec<(String, Vec<String>)> {
        let lists = self.inner.friend_lists.read().await;
        lists
            .iter()
            .map(|(k, orswot)| (k.clone(), orswot.read().val.into_iter().collect()))
            .collect()
    }

    /// Merge an incoming friend-list CRDT blob from a peer node (Phase C).
    pub async fn merge_friend_list(&self, nick: &str, blob: &[u8]) {
        let key = nick.to_ascii_lowercase();
        match bincode::serde::decode_from_slice::<Orswot<String, NodeId>, _>(
            blob,
            bincode::config::standard(),
        ) {
            Ok((remote, _)) => {
                let mut lists = self.inner.friend_lists.write().await;
                let local = lists.entry(key.clone()).or_insert_with(Orswot::new);
                local.merge(remote);
                let snapshot = local.clone();
                drop(lists);
                self.persist_friend_list(&key, &snapshot);
            }
            Err(e) => {
                warn!(nick = %nick, error = %e, "merge_friend_list: failed to decode blob");
            }
        }
    }

    fn persist_friend_list(&self, nick_lower: &str, orswot: &Orswot<String, NodeId>) {
        match bincode::serde::encode_to_vec(orswot, bincode::config::standard()) {
            Ok(blob) => self.enqueue_write(WriteOp::FriendList {
                nick_lower: nick_lower.to_string(),
                blob,
            }),
            Err(e) => {
                warn!(nick = %nick_lower, error = %e, "PersistentState: failed to encode friend list");
            }
        }
    }

    /// Persist a friend list from already-encoded bytes (avoids re-encoding).
    fn persist_friend_list_bytes(&self, nick_lower: &str, blob: Vec<u8>) {
        self.enqueue_write(WriteOp::FriendList {
            nick_lower: nick_lower.to_string(),
            blob,
        });
    }
}

// ---------------------------------------------------------------------------
// Silence list operations
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Add a target to a nick's silence set. Returns `true` if added.
    /// The reason is stored via a companion LWW entry.
    /// Writes through to SQLite.
    pub async fn add_silence(&self, nick: &str, target: &str, reason: Option<&str>) -> bool {
        let key = nick.to_ascii_lowercase();
        let target_lower = target.to_ascii_lowercase();

        let mut sets = self.inner.silence_sets.write().await;
        let orswot = sets.entry(key.clone()).or_insert_with(Orswot::new);

        let read_ctx = orswot.contains(&target_lower);
        if read_ctx.val {
            return false;
        }

        let add_ctx = orswot.read_ctx().derive_add_ctx(self.inner.node_id.clone());
        let op = orswot.add(target_lower.clone(), add_ctx);
        orswot.apply(op);

        let blob_result = bincode::serde::encode_to_vec(orswot, bincode::config::standard());
        drop(sets);

        if let Ok(blob) = blob_result {
            self.persist_silence_set_bytes(&key, blob.clone());
            self.gossip(&format!("silence:{key}"), blob).await;
        }

        // Store reason via LWW.
        let reason_key = format!("{}:{}", key, target_lower);
        let now = now_unix();
        let entry = LwwEntry {
            value: reason.map(|s| s.to_string()),
            clock: now,
            node_id: self.inner.node_id.clone(),
        };
        self.inner
            .silence_reasons
            .write()
            .await
            .insert(reason_key.clone(), entry.clone());
        self.persist_silence_reason(&reason_key, &entry);

        true
    }

    /// Remove a target from a nick's silence set. Returns `true` if removed.
    /// Writes through to SQLite.
    pub async fn remove_silence(&self, nick: &str, target: &str) -> bool {
        let key = nick.to_ascii_lowercase();
        let target_lower = target.to_ascii_lowercase();

        let mut sets = self.inner.silence_sets.write().await;
        let Some(orswot) = sets.get_mut(&key) else {
            return false;
        };

        let read_ctx = orswot.contains(&target_lower);
        if !read_ctx.val {
            return false;
        }

        let rm_ctx = read_ctx.derive_rm_ctx();
        let op = orswot.rm(target_lower.clone(), rm_ctx);
        orswot.apply(op);

        let blob_result = bincode::serde::encode_to_vec(orswot, bincode::config::standard());
        drop(sets);

        if let Ok(blob) = blob_result {
            self.persist_silence_set_bytes(&key, blob.clone());
            self.gossip(&format!("silence:{key}"), blob).await;
        }

        // Remove reason.
        let reason_key = format!("{}:{}", key, target_lower);
        self.inner.silence_reasons.write().await.remove(&reason_key);
        self.enqueue_write(WriteOp::DeleteSilenceReason {
            owner_target_key: reason_key,
        });

        true
    }

    /// Return all silenced targets for a nick, with optional reasons.
    pub async fn get_silence_list(&self, nick: &str) -> Vec<(String, Option<String>)> {
        let key = nick.to_ascii_lowercase();
        let sets = self.inner.silence_sets.read().await;
        let targets: Vec<String> = match sets.get(&key) {
            Some(orswot) => orswot.read().val.into_iter().collect(),
            None => return vec![],
        };
        drop(sets);

        let reasons = self.inner.silence_reasons.read().await;
        targets
            .into_iter()
            .map(|target| {
                let reason_key = format!("{}:{}", key, target);
                let reason = reasons.get(&reason_key).and_then(|e| e.value.clone());
                (target, reason)
            })
            .collect()
    }

    /// Iterate all silence sets (for NickServ startup import).
    ///
    /// Acquires `silence_sets` and `silence_reasons` exactly once each,
    /// collects all entries in a single pass, then drops both locks (ON-9).
    #[allow(dead_code)]
    pub async fn all_silence_lists(&self) -> Vec<(String, Vec<(String, Option<String>)>)> {
        // Acquire both locks once and hold them only for the duration of the
        // in-memory snapshot — no per-user lock round-trip.
        let sets = self.inner.silence_sets.read().await;
        let reasons = self.inner.silence_reasons.read().await;

        sets.iter()
            .map(|(owner_key, orswot)| {
                let targets: Vec<(String, Option<String>)> = orswot
                    .read()
                    .val
                    .into_iter()
                    .map(|target| {
                        let reason_key = format!("{}:{}", owner_key, target);
                        let reason = reasons.get(&reason_key).and_then(|e| e.value.clone());
                        (target, reason)
                    })
                    .collect();
                (owner_key.clone(), targets)
            })
            .collect()
    }

    /// Merge an incoming silence-set CRDT blob from a peer node (Phase C).
    pub async fn merge_silence_set(&self, nick: &str, blob: &[u8]) {
        let key = nick.to_ascii_lowercase();
        match bincode::serde::decode_from_slice::<Orswot<String, NodeId>, _>(
            blob,
            bincode::config::standard(),
        ) {
            Ok((remote, _)) => {
                let mut sets = self.inner.silence_sets.write().await;
                let local = sets.entry(key.clone()).or_insert_with(Orswot::new);
                local.merge(remote);
                let snapshot = local.clone();
                drop(sets);
                self.persist_silence_set(&key, &snapshot);
            }
            Err(e) => {
                warn!(nick = %nick, error = %e, "merge_silence_set: failed to decode blob");
            }
        }
    }

    fn persist_silence_set(&self, nick_lower: &str, orswot: &Orswot<String, NodeId>) {
        match bincode::serde::encode_to_vec(orswot, bincode::config::standard()) {
            Ok(blob) => self.enqueue_write(WriteOp::SilenceSet {
                nick_lower: nick_lower.to_string(),
                blob,
            }),
            Err(e) => {
                warn!(nick = %nick_lower, error = %e, "PersistentState: failed to encode silence set");
            }
        }
    }

    /// Persist a silence set from already-encoded bytes (avoids re-encoding).
    fn persist_silence_set_bytes(&self, nick_lower: &str, blob: Vec<u8>) {
        self.enqueue_write(WriteOp::SilenceSet {
            nick_lower: nick_lower.to_string(),
            blob,
        });
    }

    fn persist_silence_reason(&self, owner_target_key: &str, entry: &LwwEntry<Option<String>>) {
        self.enqueue_write(WriteOp::SilenceReason {
            owner_target_key: owner_target_key.to_string(),
            reason: entry.value.clone(),
            clock: entry.clock as i64,
            node_id: entry.node_id.clone(),
        });
    }
}

// ---------------------------------------------------------------------------
// SHA3 hashing for anti-entropy (Phase C)
// ---------------------------------------------------------------------------

impl PersistentState {
    /// Compute a SHA3-256 hash of the serialised ban list for `channel`.
    /// Returns `None` if the channel has no ban list.
    #[allow(dead_code)]
    pub async fn ban_list_hash(&self, channel: &str) -> Option<[u8; 32]> {
        use sha3::{Digest, Sha3_256};
        let blob = self.export_ban_list(channel).await?;
        let mut h = Sha3_256::new();
        h.update(&blob);
        Some(h.finalize().into())
    }

    /// Compute SHA3-256 hashes for every CRDT in the store.
    ///
    /// Returns a map from crdt_id → 32-byte hash.  Used for anti-entropy:
    /// the requesting node sends its map; the responder compares and returns
    /// blobs for any diverged CRDTs.
    pub async fn all_crdt_hashes(&self) -> HashMap<String, Vec<u8>> {
        // Phase 1 (async, fast): acquire all five read locks simultaneously,
        // clone all snapshots, then release all guards at once.
        // Holding them concurrently is cheaper than five sequential
        // acquire→clone→release cycles, which each create a separate
        // contention window for writers.
        let (ban_entries, nick_entries, chan_entries, friend_entries, silence_entries) = {
            let ban_guard = self.inner.ban_lists.read().await;
            let nick_guard = self.inner.nick_registrations.read().await;
            let chan_guard = self.inner.channel_registrations.read().await;
            let friend_guard = self.inner.friend_lists.read().await;
            let silence_guard = self.inner.silence_sets.read().await;

            let ban_entries: Vec<(String, Orswot<BanMask, NodeId>)> = ban_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let nick_entries: Vec<(String, LwwEntry<NickRecord>)> = nick_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let chan_entries: Vec<(String, LwwEntry<ChannelRecord>)> = chan_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let friend_entries: Vec<(String, Orswot<String, NodeId>)> = friend_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            let silence_entries: Vec<(String, Orswot<String, NodeId>)> = silence_guard
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            // All five guards dropped here simultaneously.
            (
                ban_entries,
                nick_entries,
                chan_entries,
                friend_entries,
                silence_entries,
            )
        };

        // Phase 2 (CPU-bound): bincode encode + SHA3 hash, off the async executor.
        tokio::task::spawn_blocking(move || {
            use sha3::{Digest, Sha3_256};
            let mut out: HashMap<String, Vec<u8>> = HashMap::new();

            for (key, orswot) in ban_entries {
                if let Ok(blob) =
                    bincode::serde::encode_to_vec(&orswot, bincode::config::standard())
                {
                    let mut h = Sha3_256::new();
                    h.update(&blob);
                    out.insert(format!("ban:{key}"), h.finalize().to_vec());
                }
            }

            for (key, entry) in nick_entries {
                if let Ok(blob) = bincode::serde::encode_to_vec(&entry, bincode::config::standard())
                {
                    let mut h = Sha3_256::new();
                    h.update(&blob);
                    out.insert(format!("nick:{key}"), h.finalize().to_vec());
                }
            }

            for (key, entry) in chan_entries {
                if let Ok(blob) = bincode::serde::encode_to_vec(&entry, bincode::config::standard())
                {
                    let mut h = Sha3_256::new();
                    h.update(&blob);
                    out.insert(format!("channel:{key}"), h.finalize().to_vec());
                }
            }

            for (key, orswot) in friend_entries {
                if let Ok(blob) =
                    bincode::serde::encode_to_vec(&orswot, bincode::config::standard())
                {
                    let mut h = Sha3_256::new();
                    h.update(&blob);
                    out.insert(format!("friend:{key}"), h.finalize().to_vec());
                }
            }

            for (key, orswot) in silence_entries {
                if let Ok(blob) =
                    bincode::serde::encode_to_vec(&orswot, bincode::config::standard())
                {
                    let mut h = Sha3_256::new();
                    h.update(&blob);
                    out.insert(format!("silence:{key}"), h.finalize().to_vec());
                }
            }

            out
        })
        .await
        .unwrap_or_default()
    }

    /// Export a single CRDT by ID as a serialised blob.
    ///
    /// crdt_id format:
    /// - `"ban:<channel_lower>"` — channel ban list
    /// - `"nick:<nick_lower>"` — nick registration LwwEntry
    /// - `"channel:<chan_lower>"` — channel registration LwwEntry
    /// - `"friend:<nick_lower>"` — friend list Orswot
    /// - `"silence:<nick_lower>"` — silence set Orswot
    pub async fn export_crdt(&self, crdt_id: &str) -> Option<Vec<u8>> {
        if let Some(key) = crdt_id.strip_prefix("ban:") {
            return self.export_ban_list(key).await;
        }
        if let Some(key) = crdt_id.strip_prefix("nick:") {
            let regs = self.inner.nick_registrations.read().await;
            let entry = regs.get(key)?;
            return bincode::serde::encode_to_vec(entry, bincode::config::standard()).ok();
        }
        if let Some(key) = crdt_id.strip_prefix("channel:") {
            let regs = self.inner.channel_registrations.read().await;
            let entry = regs.get(key)?;
            return bincode::serde::encode_to_vec(entry, bincode::config::standard()).ok();
        }
        if let Some(key) = crdt_id.strip_prefix("friend:") {
            let lists = self.inner.friend_lists.read().await;
            let orswot = lists.get(key)?;
            return bincode::serde::encode_to_vec(orswot, bincode::config::standard()).ok();
        }
        if let Some(key) = crdt_id.strip_prefix("silence:") {
            let sets = self.inner.silence_sets.read().await;
            let orswot = sets.get(key)?;
            return bincode::serde::encode_to_vec(orswot, bincode::config::standard()).ok();
        }
        None
    }

    /// Merge a single CRDT blob received from a peer (anti-entropy or delta gossip).
    ///
    /// Dispatches to the appropriate merge method based on crdt_id prefix.
    pub async fn merge_crdt(&self, crdt_id: &str, blob: &[u8]) {
        if let Some(key) = crdt_id.strip_prefix("ban:") {
            self.merge_ban_list(key, blob).await;
            return;
        }
        if let Some(key) = crdt_id.strip_prefix("nick:") {
            match bincode::serde::decode_from_slice::<LwwEntry<NickRecord>, _>(
                blob,
                bincode::config::standard(),
            ) {
                Ok((entry, _)) => self.merge_nick(key, entry).await,
                Err(e) => warn!(crdt_id, error = %e, "merge_crdt: failed to decode nick entry"),
            }
            return;
        }
        if let Some(key) = crdt_id.strip_prefix("channel:") {
            match bincode::serde::decode_from_slice::<LwwEntry<ChannelRecord>, _>(
                blob,
                bincode::config::standard(),
            ) {
                Ok((entry, _)) => self.merge_channel(key, entry).await,
                Err(e) => warn!(crdt_id, error = %e, "merge_crdt: failed to decode channel entry"),
            }
            return;
        }
        if let Some(key) = crdt_id.strip_prefix("friend:") {
            self.merge_friend_list(key, blob).await;
            return;
        }
        if let Some(key) = crdt_id.strip_prefix("silence:") {
            self.merge_silence_set(key, blob).await;
            return;
        }
        warn!(crdt_id, "merge_crdt: unknown crdt_id prefix");
    }
}

// ---------------------------------------------------------------------------
// Migrations (inline SQL)
// ---------------------------------------------------------------------------

async fn run_migrations(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ban_lists (
            channel     TEXT    NOT NULL,
            crdt_blob   BLOB    NOT NULL,
            updated_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (channel)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS nick_registrations (
            nick_lower  TEXT    NOT NULL,
            data_blob   BLOB    NOT NULL,
            clock       INTEGER NOT NULL DEFAULT 0,
            node_id     TEXT    NOT NULL DEFAULT '',
            updated_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (nick_lower)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS channel_registrations (
            channel_lower   TEXT    NOT NULL,
            data_blob       BLOB    NOT NULL,
            clock           INTEGER NOT NULL DEFAULT 0,
            node_id         TEXT    NOT NULL DEFAULT '',
            updated_at      INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (channel_lower)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS friend_lists (
            nick_lower  TEXT    NOT NULL,
            crdt_blob   BLOB    NOT NULL,
            updated_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (nick_lower)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS silence_sets (
            nick_lower  TEXT    NOT NULL,
            crdt_blob   BLOB    NOT NULL,
            updated_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (nick_lower)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS silence_reasons (
            owner_target_key    TEXT    NOT NULL,
            reason              TEXT,
            clock               INTEGER NOT NULL DEFAULT 0,
            node_id             TEXT    NOT NULL DEFAULT '',
            updated_at          INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
            PRIMARY KEY (owner_target_key)
        )",
    )
    .execute(pool)
    .await?;

    debug!("PersistentState: migrations complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ban_add_remove_roundtrip() {
        let ps = PersistentState::open(Path::new(":memory:"), "test-node")
            .await
            .unwrap();

        assert!(ps.add_ban("#test", "*!*@badguy.net".to_string()).await);
        assert!(!ps.add_ban("#test", "*!*@badguy.net".to_string()).await); // idempotent

        let bans = ps.get_bans("#test").await;
        assert_eq!(bans.len(), 1);
        assert!(ps.is_banned("#test", "*", Some("*@badguy.net")).await);

        assert!(ps.remove_ban("#test", "*!*@badguy.net").await);
        assert!(ps.get_bans("#test").await.is_empty());
    }

    #[tokio::test]
    async fn nick_registration_roundtrip() {
        let ps = PersistentState::open(Path::new(":memory:"), "test-node")
            .await
            .unwrap();

        ps.upsert_nick(NickRecord {
            nick: "Alice".to_string(),
            scram_stored_key: Some("aabbcc".repeat(8).chars().take(64).collect()),
            scram_server_key: Some("ddeeff".repeat(8).chars().take(64).collect()),
            scram_salt: Some("deadbeef".repeat(4)),
            scram_iterations: Some(600_000),
            bcrypt_hash: Some("$2b$12$fakehashfortesting".to_string()),
            pubkey_hex: None,
            registered_at: 1000,
            reputation: 5,
            capabilities: vec![],
        })
        .await;

        let rec = ps.get_nick("alice").await.unwrap();
        assert_eq!(rec.nick, "Alice");
        assert_eq!(rec.reputation, 5);

        ps.remove_nick("alice").await;
        assert!(ps.get_nick("alice").await.is_none());
    }

    #[tokio::test]
    async fn friend_list_roundtrip() {
        let ps = PersistentState::open(Path::new(":memory:"), "test-node")
            .await
            .unwrap();

        assert!(ps.add_friend("alice", "bob").await);
        assert!(!ps.add_friend("alice", "bob").await); // idempotent
        assert!(ps.add_friend("alice", "carol").await);

        let friends = ps.get_friends("alice").await;
        assert_eq!(friends.len(), 2);
        assert!(friends.contains(&"bob".to_string()));
        assert!(friends.contains(&"carol".to_string()));

        assert!(ps.remove_friend("alice", "bob").await);
        assert!(!ps.remove_friend("alice", "bob").await); // already gone
        assert_eq!(ps.get_friends("alice").await, vec!["carol"]);
    }

    #[tokio::test]
    async fn silence_list_roundtrip() {
        let ps = PersistentState::open(Path::new(":memory:"), "test-node")
            .await
            .unwrap();

        assert!(ps.add_silence("alice", "spammer", Some("too noisy")).await);
        assert!(!ps.add_silence("alice", "spammer", None).await); // idempotent
        assert!(ps.add_silence("alice", "bot", None).await);

        let list = ps.get_silence_list("alice").await;
        assert_eq!(list.len(), 2);
        let spammer = list.iter().find(|(t, _)| t == "spammer").unwrap();
        assert_eq!(spammer.1.as_deref(), Some("too noisy"));

        assert!(ps.remove_silence("alice", "spammer").await);
        assert!(!ps.remove_silence("alice", "spammer").await); // already gone
        assert_eq!(ps.get_silence_list("alice").await.len(), 1);
    }

    #[test]
    fn glob_ban_mask() {
        assert!(glob_match("*!*@badguy.net", "*!*@badguy.net"));
        assert!(glob_match("*bot*", "spambot"));
        assert!(!glob_match("*bot", "botnet"));
    }

    /// Verify that `compact_crdt_tombstones` preserves all live members and
    /// allows further mutations after compaction.
    #[tokio::test]
    async fn compact_tombstones_preserves_live_members() {
        let ps = PersistentState::open(Path::new(":memory:"), "test-node")
            .await
            .unwrap();

        // Ban list: add two, remove one, compact, check survivor.
        ps.add_ban("#chan", "*!*@bad1.net".to_string()).await;
        ps.add_ban("#chan", "*!*@bad2.net".to_string()).await;
        ps.remove_ban("#chan", "*!*@bad1.net").await;
        ps.compact_crdt_tombstones().await;
        let bans = ps.get_bans("#chan").await;
        assert_eq!(
            bans.len(),
            1,
            "ban list should have 1 survivor after compaction"
        );
        assert!(bans.contains(&"*!*@bad2.net".to_string()));
        // Further adds must still work after compaction.
        assert!(ps.add_ban("#chan", "*!*@bad3.net".to_string()).await);
        assert_eq!(ps.get_bans("#chan").await.len(), 2);

        // Friend list: add two, remove one, compact, check survivor.
        ps.add_friend("alice", "bob").await;
        ps.add_friend("alice", "carol").await;
        ps.remove_friend("alice", "bob").await;
        ps.compact_crdt_tombstones().await;
        let friends = ps.get_friends("alice").await;
        assert_eq!(
            friends.len(),
            1,
            "friend list should have 1 survivor after compaction"
        );
        assert!(friends.contains(&"carol".to_string()));

        // Silence set: add two, remove one, compact, check survivor.
        ps.add_silence("alice", "spammer", Some("noisy")).await;
        ps.add_silence("alice", "bot", None).await;
        ps.remove_silence("alice", "spammer").await;
        ps.compact_crdt_tombstones().await;
        let silenced = ps.get_silence_list("alice").await;
        assert_eq!(
            silenced.len(),
            1,
            "silence set should have 1 survivor after compaction"
        );
        assert_eq!(silenced[0].0, "bot");
    }
}
