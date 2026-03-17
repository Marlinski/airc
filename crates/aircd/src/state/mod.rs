//! Shared server state — the single source of truth for clients and channels.
//!
//! All mutations flow through [`SharedState`] methods so the interface can be
//! extracted into a trait later for testing or alternative backends.
//!
//! # Client identity model
//!
//! There are 64 user shards: `users: [RwLock<HashMap<ClientId, Client>>; 64]`.
//! A client with `ClientId(n)` lives in shard `n % 64`.  A local client has
//! `ClientKind::Local { tx, cancel, server_name }` and can be written to directly.
//! A remote client has `ClientKind::Remote { node_id }` and is only reachable via
//! the relay.
//!
//! A secondary `nick_index: RwLock<HashMap<String, ClientId>>` maps lowercase
//! nick strings to `ClientId`, enabling O(1) lookups instead of O(n) scans.
//! The index is kept in sync in every method that inserts, removes, or renames
//! clients.  Lock ordering when holding both: acquire `nick_index` write THEN
//! `users` shard write.
//!
//! # Channel locking model
//!
//! `Inner.channels` is a two-level structure:
//!
//! ```text
//! RwLock<HashMap<String, Arc<RwLock<Channel>>>>
//! ```
//!
//! The outer map lock is held only long enough to clone an `Arc<RwLock<Channel>>`
//! (a pointer copy).  Per-channel mutations then acquire the inner lock while the
//! map lock is already released.  This eliminates the "lock the world" pattern
//! where a single `channels.write()` blocks every concurrent PRIVMSG, JOIN, PART,
//! and WHO across all channels.
//!
//! Channel removal requires a short write lock on the outer map after emptying
//! the channel; a double-check under that write lock prevents a TOCTOU race where
//! two concurrent PARTs would both see an empty channel and both try to remove it.

pub mod channel;
pub mod relay;
pub mod stats;
pub mod user;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use thiserror::Error;
use tokio::sync::RwLock;

use crate::channel::Channel;
use crate::client::{Client, ClientHandle, ClientId};
use crate::config::ServerConfig;
use crate::persist::PersistentState;
use crate::relay::Relay;
use crate::services::ServicesState;
use crate::web;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors related to nickname operations.
#[derive(Debug, Clone, Error)]
pub enum NickError {
    #[error("nickname already in use")]
    InUse,
    #[error("invalid nickname")]
    Invalid,
}

/// Result of `check_channel_send` — the hot path for PRIVMSG/NOTICE fan-out.
pub enum ChannelSendResult {
    /// Channel does not exist.
    NoSuchChannel,
    /// +n mode: sender is not a member and no-external is set.
    NoExternal,
    /// +m mode: sender is neither voiced nor opped.
    Moderated,
    /// All checks passed — contains the local member handles to fan out to
    /// (sender excluded).
    Ok(Vec<ClientHandle>),
}

// ---------------------------------------------------------------------------
// Prometheus metrics snapshot
// ---------------------------------------------------------------------------

/// Lightweight snapshot for the Prometheus `/metrics` scrape endpoint.
/// Avoids the topic/mode string allocations and sort that `stats()` performs.
pub struct PrometheusStats {
    pub users_online: u64,
    pub channels_active: u64,
    pub uptime_seconds: u64,
    /// `(channel_name, member_count)` for every active channel.
    pub channel_counts: Vec<(String, u64)>,
}

// ---------------------------------------------------------------------------
// Stats cache — ON-8
// ---------------------------------------------------------------------------

/// TTL for the stats cache used by `/api/channels`, `/api/stats`, and
/// `/metrics`.  10,000 channels × lock-acquisition overhead per scrape is
/// eliminated after the first call within any 1-second window.
pub(super) const STATS_CACHE_TTL_MS: u64 = 1_000;

/// Cached output of the O(channels) stats computation.
/// Stored in `Inner` and lazily refreshed after `STATS_CACHE_TTL_MS`.
pub(super) struct StatsCache {
    pub(super) users_online: u64,
    pub(super) channels_active: u64,
    pub(super) channel_info: Vec<web::ChannelInfo>, // for /api/channels
    pub(super) channel_counts: Vec<(String, u64)>,  // for /metrics
    pub(super) computed_at: Option<Instant>,
}

impl StatsCache {
    pub(super) const fn new() -> Self {
        Self {
            users_online: 0,
            channels_active: 0,
            channel_info: Vec::new(),
            channel_counts: Vec::new(),
            computed_at: None,
        }
    }

    pub(super) fn is_fresh(&self) -> bool {
        self.computed_at
            .is_some_and(|t| t.elapsed().as_millis() < STATS_CACHE_TTL_MS as u128)
    }
}

// ---------------------------------------------------------------------------
// Inner state (behind the Arc)
// ---------------------------------------------------------------------------

/// Number of user-map shards.  Must be a power of two.
pub(super) const NUM_SHARDS: usize = 64;

pub(super) struct Inner {
    /// Sharded user registry: ClientId(n) lives in shard n % NUM_SHARDS.
    pub(super) users: [RwLock<HashMap<ClientId, Client>>; NUM_SHARDS],
    /// Two-level channel map: outer map lock is held briefly to clone the Arc;
    /// per-channel mutations use the inner RwLock.
    pub(super) channels: RwLock<HashMap<String, Arc<RwLock<Channel>>>>,
    /// Secondary index: lowercase nick → ClientId for O(1) nick lookups.
    pub(super) nick_index: RwLock<HashMap<String, ClientId>>,
    /// Secondary index: ClientId → set of channel keys (lowercase) the client is in.
    pub(super) membership_index: RwLock<HashMap<ClientId, HashSet<String>>>,
    pub(super) next_id: AtomicU64,
    pub(super) config: ServerConfig,
    pub(super) relay: Arc<dyn Relay>,
    pub(super) started_at: Instant,
    /// Embedded NickServ / ChanServ services. Set after SharedState is created
    /// to avoid the chicken-and-egg dependency (ServicesState needs SharedState).
    pub(super) services: tokio::sync::OnceCell<Arc<ServicesState>>,
    /// CRDT-backed persistent state (ban lists, nick/channel registrations).
    /// Set once after `SharedState::new()` via `set_persistent()`.
    pub(super) persistent: tokio::sync::OnceCell<PersistentState>,
    /// Cached stats for `/api/channels`, `/api/stats`, and `/metrics`.
    /// Refreshed at most once per `STATS_CACHE_TTL_MS` milliseconds.
    pub(super) stats_cache: RwLock<StatsCache>,
}

// ---------------------------------------------------------------------------
// SharedState
// ---------------------------------------------------------------------------

/// Thread-safe, cheaply cloneable handle to all server state.
#[derive(Clone)]
pub struct SharedState {
    pub(super) inner: Arc<Inner>,
}

impl SharedState {
    /// Create a fresh server state from the given config and relay backend.
    pub fn new(config: ServerConfig, relay: Arc<dyn Relay>) -> Self {
        Self {
            inner: Arc::new(Inner {
                users: std::array::from_fn(|_| RwLock::new(HashMap::new())),
                nick_index: RwLock::new(HashMap::new()),
                membership_index: RwLock::new(HashMap::new()),
                channels: RwLock::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                config,
                relay,
                started_at: Instant::now(),
                services: tokio::sync::OnceCell::new(),
                persistent: tokio::sync::OnceCell::new(),
                stats_cache: RwLock::new(StatsCache::new()),
            }),
        }
    }

    /// Return a reference to the shard that owns `id`.
    #[inline]
    pub(super) fn user_shard(&self, id: ClientId) -> &RwLock<HashMap<ClientId, Client>> {
        &self.inner.users[(id.0 % NUM_SHARDS as u64) as usize]
    }

    /// Access the relay backend.
    pub fn relay(&self) -> &dyn Relay {
        &*self.inner.relay
    }

    /// Initialize embedded services. Must be called once after `SharedState::new()`.
    pub fn set_services(&self, services: Arc<ServicesState>) {
        let _ = self.inner.services.set(services);
    }

    /// Access embedded services (NickServ / ChanServ).
    ///
    /// Returns `None` if services have not been initialized yet.
    pub fn services(&self) -> Option<Arc<ServicesState>> {
        self.inner.services.get().cloned()
    }

    /// Initialize CRDT-backed persistent state. Must be called once after
    /// `SharedState::new()`, before serving any connections.
    pub fn set_persistent(&self, ps: PersistentState) {
        let _ = self.inner.persistent.set(ps);
    }

    /// Access CRDT-backed persistent state.
    ///
    /// Returns `None` if `set_persistent()` has not been called yet.
    pub fn persistent(&self) -> Option<&PersistentState> {
        self.inner.persistent.get()
    }

    // -- Identity -----------------------------------------------------------

    /// Allocate the next unique client ID (local clients only).
    pub fn next_client_id(&self) -> ClientId {
        ClientId(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The server's configured hostname.
    pub fn server_name(&self) -> &str {
        &self.inner.config.server_name
    }

    /// Borrow the full server config.
    pub fn config(&self) -> &ServerConfig {
        &self.inner.config
    }
}

// ---------------------------------------------------------------------------
// Helpers (used by multiple submodules)
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hash — used to derive a stable channel_id from its name.
pub(super) fn fnv1a_hash(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut hash = FNV_OFFSET;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Basic IRC nickname validation — delegates to the shared protocol library.
pub(super) fn is_valid_nick(nick: &str) -> bool {
    airc_shared::validate::is_valid_nick(nick)
}
