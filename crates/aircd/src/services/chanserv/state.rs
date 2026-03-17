//! ChanServ shared state — the central data store shared by all ChanServ modules.
//!
//! All persistent channel state is stored exclusively in the CRDT-backed
//! `PersistentState` (SQLite write-through).  There is no longer any JSON
//! file persistence.
//!
//! The in-memory `channels` map is a hot-path read cache populated from the
//! CRDT store at startup via `init_from_persist()`.

use std::collections::HashMap;

use tokio::sync::RwLock;
use tracing::info;

use super::types::RegisteredChannel;
use crate::util::glob_match;

/// Shared inner state for all ChanServ modules.
///
/// All modules hold an `Arc<ChanServState>`.  Rather than locking the map
/// directly, callers use the typed accessor methods which encapsulate lock
/// scope and write-through to the CRDT persistent store.
pub struct ChanServState {
    /// Registered channels, keyed by lowercase channel name.
    /// Hot-path read cache seeded from CRDT at startup.
    channels: RwLock<HashMap<String, RegisteredChannel>>,
    /// Optional CRDT persistent state for write-through.
    persist: Option<crate::persist::PersistentState>,
}

impl ChanServState {
    /// Create a new `ChanServState`. No disk I/O is performed here; call
    /// `init_from_persist()` after wiring in the `PersistentState` to seed
    /// from the CRDT store.
    pub fn new() -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            persist: None,
        }
    }

    /// Attach a `PersistentState` for CRDT write-through.
    pub fn set_persistent(&mut self, ps: crate::persist::PersistentState) {
        self.persist = Some(ps);
    }

    /// Seed in-memory channels from CRDT persistent state (called once at startup).
    pub async fn init_from_persist(&self) {
        let Some(ref ps) = self.persist else {
            return;
        };
        let records = ps.all_channels().await;
        if records.is_empty() {
            return;
        }
        let mut channels = self.channels.write().await;
        for (_key, rec) in records {
            let name_lower = rec.name.to_ascii_lowercase();
            channels
                .entry(name_lower)
                .or_insert_with(|| RegisteredChannel {
                    name: rec.name,
                    founder: rec.founder,
                    min_reputation: rec.min_reputation,
                    bans: Vec::new(),
                    description: rec.description,
                });
        }
        info!(
            count = channels.len(),
            "ChanServ: loaded channels from persistent state"
        );
    }

    // -- Channel queries ----------------------------------------------------

    /// Look up a registered channel by name (case-insensitive).
    pub async fn get_channel(&self, name: &str) -> Option<RegisteredChannel> {
        self.channels
            .read()
            .await
            .get(&name.to_ascii_lowercase())
            .cloned()
    }

    /// Check if a channel is registered.
    #[allow(dead_code)]
    pub async fn is_registered(&self, name: &str) -> bool {
        self.channels
            .read()
            .await
            .contains_key(&name.to_ascii_lowercase())
    }

    /// Check if a nick is the founder of a registered channel.
    pub async fn is_founder(&self, channel: &str, nick: &str) -> bool {
        let key = channel.to_ascii_lowercase();
        self.channels
            .read()
            .await
            .get(&key)
            .is_some_and(|reg| reg.founder == nick.to_ascii_lowercase())
    }

    /// Register a new channel. Returns `false` if already registered.
    pub async fn register_channel(&self, channel: RegisteredChannel) -> bool {
        let key = channel.name.to_ascii_lowercase();
        // Capture the persist record before inserting; drop the write lock
        // before calling upsert_channel so that the async Redis gossip publish
        // does not block concurrent readers (e.g. JOIN checks) for its duration.
        let record = channel_to_record(&channel);
        {
            let mut channels = self.channels.write().await;
            if channels.contains_key(&key) {
                return false;
            }
            channels.insert(key, channel);
        }
        if let Some(ref ps) = self.persist {
            ps.upsert_channel(record).await;
        }
        true
    }

    /// Modify a registered channel's settings. Returns `false` if not registered.
    pub async fn modify_channel<F>(&self, name: &str, f: F) -> bool
    where
        F: FnOnce(&mut RegisteredChannel),
    {
        let key = name.to_ascii_lowercase();
        // Apply the mutation and snapshot the record needed for persistence,
        // then drop the write lock before the async upsert so concurrent JOIN
        // checks are not blocked during the Redis gossip publish.
        let record = {
            let mut channels = self.channels.write().await;
            let Some(reg) = channels.get_mut(&key) else {
                return false;
            };
            f(reg);
            channel_to_record(reg)
        };
        if let Some(ref ps) = self.persist {
            ps.upsert_channel(record).await;
        }
        true
    }

    // -- Join checking ------------------------------------------------------

    /// Check if a user is allowed to join a registered channel.
    /// Returns `Ok(())` if allowed, `Err(reason)` if denied.
    ///
    /// Ban check and reputation check are performed in-place under a single
    /// read lock to avoid cloning the ban list.
    pub async fn check_join(
        &self,
        channel_name: &str,
        nick: &str,
        reputation: i64,
    ) -> Result<(), String> {
        let key = channel_name.to_ascii_lowercase();
        let nick_lower = nick.to_ascii_lowercase();

        let channels = self.channels.read().await;
        let Some(reg) = channels.get(&key) else {
            return Ok(()); // Unregistered channel — anyone can join.
        };

        for pattern in &reg.bans {
            if nick_lower == *pattern || glob_match(pattern, &nick_lower) {
                return Err("You are banned from this channel.".to_string());
            }
        }

        let min_reputation = reg.min_reputation;
        drop(channels);

        if reputation < min_reputation {
            return Err(format!(
                "Minimum reputation of {} required (you have {}).",
                min_reputation, reputation
            ));
        }

        Ok(())
    }
}

impl Default for ChanServState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn channel_to_record(ch: &RegisteredChannel) -> crate::persist::ChannelRecord {
    crate::persist::ChannelRecord {
        name: ch.name.clone(),
        founder: ch.founder.clone(),
        min_reputation: ch.min_reputation,
        description: ch.description.clone(),
    }
}
