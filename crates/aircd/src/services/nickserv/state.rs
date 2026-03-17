//! NickServ shared state — the central data store shared by all NickServ modules.
//!
//! All persistent state (identities, friend lists, silence lists) is stored
//! exclusively in the CRDT-backed `PersistentState` (SQLite write-through).
//! There is no longer any JSON file persistence.
//!
//! The in-memory `identities` map is a hot-path read cache populated from the
//! CRDT store at startup via `init_from_persist()`.  Friend lists and silence
//! lists are **not** cached in memory here — they are always read from the CRDT
//! store directly, since they are not on any hot path.
//!
//! ## Lock choice: `std::sync::RwLock` for `identities`
//!
//! `identities` uses a standard (non-async) `RwLock` for two reasons:
//!
//! 1. **No await across the guard** — every access drops the guard before the
//!    next `.await`, so there is no risk of holding the lock across a yield
//!    point.
//! 2. **Sync lookup in SASL** — the `PasswordLookup` callback passed to SASL
//!    mechanisms is a sync `Fn`.  It uses `block_in_place` + `blocking_read`
//!    to perform a single-entry lookup without cloning the whole map.

use std::collections::HashMap;
use std::sync::RwLock;

use dashmap::DashMap;
use tokio::sync::RwLock as AsyncRwLock;
use tracing::info;

use super::persist::now_unix;
use super::types::{Identity, PendingChallenge, SilenceEntry};

/// Challenges older than this are considered expired and will be evicted.
const CHALLENGE_TTL_SECS: u64 = 300; // 5 minutes

/// Shared inner state for all NickServ modules.
///
/// All modules hold an `Arc<NickServState>`.  Rather than locking the maps
/// directly, callers use the typed accessor methods which encapsulate lock
/// scope and write-through to the CRDT persistent store.
pub struct NickServState {
    /// Registered identities, keyed by lowercase nick.
    /// Hot-path read cache seeded from CRDT at startup.
    /// Uses `std::sync::RwLock` — no guard is ever held across an `.await`
    /// point, and SASL needs sync access for single-entry lookup.
    pub(crate) identities: RwLock<HashMap<String, Identity>>,
    /// Active challenges for keypair auth, keyed by client nick (lowercase).
    challenges: AsyncRwLock<HashMap<String, PendingChallenge>>,
    /// Rate-limit: key is "sender_lower:action:target_lower", value is unix timestamp.
    /// Uses `DashMap` for lock-free reads and fine-grained sharded writes (LOCK-14).
    rate_limits: DashMap<String, u64>,
    /// Shared server state — used for GHOST/KILL and CRDT access.
    state: crate::state::SharedState,
}

impl NickServState {
    /// Create a new `NickServState`. No disk I/O is performed here; call
    /// `init_from_persist()` after construction to seed from the CRDT store.
    pub fn new(state: crate::state::SharedState) -> Self {
        Self {
            identities: RwLock::new(HashMap::new()),
            challenges: AsyncRwLock::new(HashMap::new()),
            rate_limits: DashMap::new(),
            state,
        }
    }

    /// Seed in-memory identities AND friend/silence caches from CRDT persistent
    /// state (called once at startup).
    pub async fn init_from_persist(&self) {
        let Some(ps) = self.state.persistent() else {
            return;
        };

        // Load nick registrations.
        let records = ps.all_nicks().await;
        if !records.is_empty() {
            let mut ids = self.identities.write().unwrap();
            for (nick_lower, rec) in &records {
                ids.entry(nick_lower.clone()).or_insert_with(|| Identity {
                    nick: rec.nick.clone(),
                    password_hash: rec.password_hash.clone(),
                    pubkey_hex: rec.pubkey_hex.clone(),
                    registered_at: rec.registered_at,
                    reputation: rec.reputation,
                    capabilities: rec.capabilities.clone(),
                });
            }
            info!(
                count = ids.len(),
                "NickServ: loaded identities from persistent state"
            );
        }
        // Friend lists and silence lists live in PersistentState; no separate
        // in-memory cache needed here.
    }

    /// Accessor for the shared server state (e.g. for GHOST/KILL operations).
    pub fn shared_state(&self) -> &crate::state::SharedState {
        &self.state
    }

    // -- Identity queries ---------------------------------------------------

    /// Look up a registered identity by nick (case-insensitive).
    pub async fn get_identity(&self, nick: &str) -> Option<Identity> {
        self.identities
            .read()
            .unwrap()
            .get(&nick.to_ascii_lowercase())
            .cloned()
    }

    /// Check if a nick is registered.
    pub async fn is_registered(&self, nick: &str) -> bool {
        self.identities
            .read()
            .unwrap()
            .contains_key(&nick.to_ascii_lowercase())
    }

    /// Insert a new identity. Returns `false` if the nick is already registered.
    pub async fn register_identity(&self, identity: Identity) -> bool {
        let nick_lower = identity.nick.to_ascii_lowercase();
        {
            let mut ids = self.identities.write().unwrap();
            if ids.contains_key(&nick_lower) {
                return false;
            }
            ids.insert(nick_lower, identity.clone());
        } // write guard dropped before .await
        if let Some(ps) = self.state.persistent() {
            ps.upsert_nick(identity_to_nick_record(&identity)).await;
        }
        true
    }

    /// Get the password hash for a nick, if it has one.
    #[allow(dead_code)]
    pub async fn get_password_hash(&self, nick: &str) -> Option<Option<String>> {
        self.identities
            .read()
            .unwrap()
            .get(&nick.to_ascii_lowercase())
            .map(|id| id.password_hash.clone())
    }

    /// Get the public key hex for a nick, if registered.
    pub async fn get_pubkey_hex(&self, nick: &str) -> Option<Option<String>> {
        self.identities
            .read()
            .unwrap()
            .get(&nick.to_ascii_lowercase())
            .map(|id| id.pubkey_hex.clone())
    }

    // -- Reputation ---------------------------------------------------------

    /// Modify reputation for a nick. Returns the new score, or `None` if not registered.
    pub async fn modify_reputation(&self, nick: &str, delta: i64) -> Option<i64> {
        let nick_lower = nick.to_ascii_lowercase();
        let updated = {
            let mut ids = self.identities.write().unwrap();
            let identity = ids.get_mut(&nick_lower)?;
            identity.reputation += delta;
            identity.clone()
        }; // write guard dropped before .await
        if let Some(ps) = self.state.persistent() {
            ps.upsert_nick(identity_to_nick_record(&updated)).await;
        }
        Some(updated.reputation)
    }

    // -- Challenges ---------------------------------------------------------

    /// Store a pending challenge for keypair auth.
    /// Evicts any stale challenges older than `CHALLENGE_TTL_SECS` before inserting.
    pub async fn set_challenge(&self, nick: &str, challenge: PendingChallenge) {
        let now = now_unix();
        let mut map = self.challenges.write().await;
        // Evict expired entries to prevent unbounded growth (MEM-2).
        map.retain(|_, c| now.saturating_sub(c.created_at) < CHALLENGE_TTL_SECS);
        map.insert(nick.to_ascii_lowercase(), challenge);
    }

    /// Remove and return a pending challenge. Returns `None` if not found or expired.
    pub async fn take_challenge(&self, nick: &str) -> Option<PendingChallenge> {
        let now = now_unix();
        let mut map = self.challenges.write().await;
        let challenge = map.remove(&nick.to_ascii_lowercase())?;
        if now.saturating_sub(challenge.created_at) >= CHALLENGE_TTL_SECS {
            // Challenge expired — discard it.
            return None;
        }
        Some(challenge)
    }

    // -- Rate limiting ------------------------------------------------------

    /// Check and record a rate-limited action.
    /// Returns `Ok(())` if allowed, `Err(seconds_remaining)` if rate-limited.
    pub async fn check_rate_limit(
        &self,
        sender: &str,
        action: &str,
        target: &str,
    ) -> Result<(), u64> {
        let key = format!(
            "{}:{}:{}",
            sender.to_ascii_lowercase(),
            action,
            target.to_ascii_lowercase()
        );
        let now = now_unix();
        let cooldown = 300; // 5 minutes

        // Evict expired entries to prevent unbounded growth.
        // DashMap::retain holds only a shard lock at a time, so it is cheaper
        // than a single global write lock over the whole map (LOCK-14).
        self.rate_limits
            .retain(|_, last| now.saturating_sub(*last) < cooldown);
        if let Some(last) = self.rate_limits.get(&key) {
            let elapsed = now.saturating_sub(*last);
            if elapsed < cooldown {
                return Err(cooldown - elapsed);
            }
        }
        self.rate_limits.insert(key, now);
        Ok(())
    }

    /// Check whether each nick in `nicks` is registered.  Returns a
    /// `HashSet<String>` of lowercase nicks that are registered.  Acquires the
    /// `identities` lock exactly once regardless of slice length.
    pub fn registered_set(&self, nicks: &[&str]) -> std::collections::HashSet<String> {
        let ids = self.identities.read().unwrap();
        nicks
            .iter()
            .filter_map(|n| {
                let lower = n.to_ascii_lowercase();
                if ids.contains_key(&lower) {
                    Some(lower)
                } else {
                    None
                }
            })
            .collect()
    }

    // -- Friend lists (delegated to PersistentState) ------------------------

    /// Add a friend to an identity's friend list. Returns `true` if added.
    ///
    /// For batched mutations prefer `batch_friend_ops`.
    #[allow(dead_code)]
    pub async fn add_friend(&self, nick: &str, friend_nick: &str) -> bool {
        let Some(ps) = self.state.persistent() else {
            return false;
        };
        ps.add_friend(nick, friend_nick).await
    }

    /// Remove a friend from an identity's friend list. Returns `true` if removed.
    ///
    /// For batched mutations prefer `batch_friend_ops`.
    #[allow(dead_code)]
    pub async fn remove_friend(&self, nick: &str, friend_nick: &str) -> bool {
        let Some(ps) = self.state.persistent() else {
            return false;
        };
        ps.remove_friend(nick, friend_nick).await
    }

    /// Apply a batch of friend-list mutations in O(1) lock acquisitions.
    /// Returns `(added, removed)` counts.
    pub async fn batch_friend_ops(
        &self,
        nick: &str,
        adds: &[String],
        removes: &[String],
    ) -> (usize, usize) {
        let Some(ps) = self.state.persistent() else {
            return (0, 0);
        };
        ps.batch_friend_ops(nick, adds, removes).await
    }

    /// Get the friend list for a nick.
    pub async fn get_friend_list(&self, nick: &str) -> Vec<String> {
        let Some(ps) = self.state.persistent() else {
            return vec![];
        };
        ps.get_friends(nick).await
    }

    // -- Silence lists (delegated to PersistentState) -----------------------

    /// Add a nick to an identity's silence list. Returns `true` if added.
    pub async fn add_silence(&self, nick: &str, target: &str, reason: Option<&str>) -> bool {
        let Some(ps) = self.state.persistent() else {
            return false;
        };
        ps.add_silence(nick, target, reason).await
    }

    /// Remove a nick from an identity's silence list. Returns `true` if removed.
    pub async fn remove_silence(&self, nick: &str, target: &str) -> bool {
        let Some(ps) = self.state.persistent() else {
            return false;
        };
        ps.remove_silence(nick, target).await
    }

    /// Get the silence list for a nick, as `SilenceEntry` values.
    pub async fn get_silence_list(&self, nick: &str) -> Vec<SilenceEntry> {
        let Some(ps) = self.state.persistent() else {
            return vec![];
        };
        ps.get_silence_list(nick)
            .await
            .into_iter()
            .map(|(target, reason)| SilenceEntry {
                nick: target,
                reason,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn identity_to_nick_record(id: &Identity) -> crate::persist::NickRecord {
    crate::persist::NickRecord {
        nick: id.nick.clone(),
        password_hash: id.password_hash.clone(),
        pubkey_hex: id.pubkey_hex.clone(),
        registered_at: id.registered_at,
        reputation: id.reputation,
        capabilities: id.capabilities.clone(),
    }
}
