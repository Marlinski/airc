//! ChanServ — channel registration and access control service.
//!
//! Composed of togglable modules:
//! - **registration**: REGISTER, INFO, SET
//! - **access**: BAN, UNBAN (+ check_join for future use)

pub mod access;
pub mod registration;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::services::ChanServModules;
use crate::services::module::{ServiceDispatcher, ServiceModule};

const PERSISTENCE_FILE: &str = "chanserv.json";

// ---------------------------------------------------------------------------
// Channel registration data
// ---------------------------------------------------------------------------

/// A registered channel's persistent metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredChannel {
    /// Channel name (canonical casing).
    pub name: String,
    /// Founder's nick (lowercase).
    pub founder: String,
    /// Minimum reputation required to join (0 = no requirement).
    pub min_reputation: i64,
    /// Banned nick patterns (lowercase).
    pub bans: Vec<String>,
    /// Description / purpose.
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// ChanServ shared state
// ---------------------------------------------------------------------------

/// Shared inner state for all ChanServ modules.
pub struct ChanServState {
    /// Registered channels, keyed by lowercase channel name.
    channels: RwLock<HashMap<String, RegisteredChannel>>,
    /// Directory for persistence files.
    data_dir: PathBuf,
}

impl ChanServState {
    /// Create a new ChanServState, loading persisted data from disk.
    pub fn new(data_dir: &Path) -> Self {
        let persistence_path = data_dir.join(PERSISTENCE_FILE);
        let channels = load_channels(&persistence_path).unwrap_or_default();
        Self {
            channels: RwLock::new(channels),
            data_dir: data_dir.to_path_buf(),
        }
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
        let channels = self.channels.read().await;
        channels
            .get(&key)
            .is_some_and(|reg| reg.founder == nick.to_ascii_lowercase())
    }

    /// Register a new channel. Returns `false` if already registered.
    pub async fn register_channel(&self, channel: RegisteredChannel) -> bool {
        let key = channel.name.to_ascii_lowercase();
        let mut channels = self.channels.write().await;
        if channels.contains_key(&key) {
            return false;
        }
        channels.insert(key, channel);
        drop(channels);
        self.persist().await;
        true
    }

    /// Modify a registered channel's settings. Returns `false` if not registered.
    pub async fn modify_channel<F>(&self, name: &str, f: F) -> bool
    where
        F: FnOnce(&mut RegisteredChannel),
    {
        let key = name.to_ascii_lowercase();
        let mut channels = self.channels.write().await;
        let Some(reg) = channels.get_mut(&key) else {
            return false;
        };
        f(reg);
        drop(channels);
        self.persist().await;
        true
    }

    // -- Join checking ------------------------------------------------------

    /// Check if a user is allowed to join a registered channel.
    /// Returns `Ok(())` if allowed, `Err(reason)` if denied.
    pub async fn check_join(
        &self,
        channel_name: &str,
        nick: &str,
        reputation: i64,
    ) -> Result<(), String> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.channels.read().await;
        let Some(reg) = channels.get(&key) else {
            return Ok(()); // Unregistered channel — anyone can join.
        };

        // Check bans.
        let nick_lower = nick.to_ascii_lowercase();
        for pattern in &reg.bans {
            if nick_lower == *pattern || glob_match(pattern, &nick_lower) {
                return Err("You are banned from this channel.".to_string());
            }
        }

        // Check reputation gate.
        if reputation < reg.min_reputation {
            return Err(format!(
                "Minimum reputation of {} required (you have {}).",
                reg.min_reputation, reputation
            ));
        }

        Ok(())
    }

    // -- Persistence --------------------------------------------------------

    async fn persist(&self) {
        let channels = self.channels.read().await;
        let path = self.data_dir.join(PERSISTENCE_FILE);
        if let Err(e) = save_channels(&path, &channels) {
            warn!(error = %e, "ChanServ: failed to persist channels");
        }
    }
}

// ---------------------------------------------------------------------------
// Module builder
// ---------------------------------------------------------------------------

/// Build the set of ChanServ modules based on config toggles.
pub fn build_modules(
    state: Arc<ChanServState>,
    modules_cfg: &ChanServModules,
) -> Vec<Box<dyn ServiceModule>> {
    let mut modules: Vec<Box<dyn ServiceModule>> = Vec::new();

    if modules_cfg.registration {
        modules.push(Box::new(registration::RegistrationModule::new(
            state.clone(),
        )));
    }
    if modules_cfg.access {
        modules.push(Box::new(access::AccessModule::new(state.clone())));
    }

    modules
}

/// Create a fully-wired ChanServ dispatcher.
pub fn create_dispatcher(
    state: Arc<ChanServState>,
    modules_cfg: &ChanServModules,
) -> ServiceDispatcher {
    let modules = build_modules(state, modules_cfg);
    ServiceDispatcher::new("ChanServ".to_string(), modules)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple glob matching: `*` matches any sequence of characters.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(idx) => {
                if i == 0 && idx != 0 {
                    return false; // Pattern doesn't start with *, but text doesn't start with part.
                }
                pos += idx + part.len();
            }
            None => return false,
        }
    }

    // If pattern ends with *, remaining text is fine. Otherwise must be at end.
    pattern.ends_with('*') || pos == text.len()
}

// ---------------------------------------------------------------------------
// Persistence (JSON file)
// ---------------------------------------------------------------------------

fn load_channels(
    path: &Path,
) -> Result<HashMap<String, RegisteredChannel>, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(path)?;
    let map: HashMap<String, RegisteredChannel> = serde_json::from_str(&data)?;
    info!(count = map.len(), path = %path.display(), "ChanServ: loaded channels");
    Ok(map)
}

fn save_channels(
    path: &Path,
    channels: &HashMap<String, RegisteredChannel>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = serde_json::to_string_pretty(channels)?;
    std::fs::write(path, data)?;
    debug!(count = channels.len(), path = %path.display(), "ChanServ: persisted channels");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_exact() {
        assert!(glob_match("alice", "alice"));
        assert!(!glob_match("alice", "bob"));
    }

    #[test]
    fn glob_wildcard() {
        assert!(glob_match("*bot", "testbot"));
        assert!(glob_match("agent*", "agent42"));
        assert!(glob_match("*mid*", "in_middle_here"));
        assert!(!glob_match("*bot", "botnet"));
    }

    #[test]
    fn glob_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }
}
