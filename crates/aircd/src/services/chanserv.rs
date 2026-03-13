//! ChanServ — channel registration and access control service.
//!
//! Provides:
//! - Channel registration (founder gets permanent operator status)
//! - Access lists (reputation-gated, invite-only enforcement)
//! - Programmatic ban management
//!
//! Future: payment gates via blockchain lookup (trait-based hook).

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use airc_shared::IrcMessage;

use super::ServiceBot;
use crate::client::ClientHandle;
use crate::state::SharedState;

const CHANSERV: &str = "ChanServ";
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
// ChanServ inner state
// ---------------------------------------------------------------------------

struct Inner {
    /// Registered channels, keyed by lowercase channel name.
    channels: RwLock<HashMap<String, RegisteredChannel>>,
}

// ---------------------------------------------------------------------------
// ChanServ
// ---------------------------------------------------------------------------

pub struct ChanServ {
    inner: Arc<Inner>,
}

impl ChanServ {
    pub fn new() -> Self {
        let channels = load_channels().unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                channels: RwLock::new(channels),
            }),
        }
    }

    /// Check if a user is allowed to join a registered channel.
    /// Returns `Ok(())` if allowed, `Err(reason)` if denied.
    pub async fn check_join(
        &self,
        channel_name: &str,
        nick: &str,
        reputation: i64,
    ) -> Result<(), String> {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
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

    /// Check if a nick is the founder of a registered channel.
    pub async fn is_founder(&self, channel_name: &str, nick: &str) -> bool {
        let key = channel_name.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;
        channels
            .get(&key)
            .is_some_and(|reg| reg.founder == nick.to_ascii_lowercase())
    }

    /// Look up a registered channel by lowercase name.
    pub async fn get_registered_channel(&self, key: &str) -> Option<RegisteredChannel> {
        self.inner.channels.read().await.get(key).cloned()
    }
}

impl ServiceBot for ChanServ {
    fn nick(&self) -> &str {
        CHANSERV
    }

    async fn handle(&self, state: &SharedState, sender: &ClientHandle, text: &str) {
        let parts: Vec<&str> = text.splitn(3, ' ').collect();
        let command = match parts.first() {
            Some(c) => c.to_ascii_uppercase(),
            None => return,
        };
        let arg1 = parts.get(1).copied();
        let rest = parts.get(2).copied(); // Everything after the second token.

        match command.as_str() {
            "REGISTER" => self.cmd_register(sender, arg1, rest).await,
            "INFO" => self.cmd_info(sender, arg1).await,
            "SET" => {
                // SET <channel> <key> <value> — need to split rest further.
                let (key, value) = match rest {
                    Some(r) => {
                        let mut kv = r.splitn(2, ' ');
                        (kv.next(), kv.next())
                    }
                    None => (None, None),
                };
                self.cmd_set(sender, arg1, key, value).await;
            }
            "BAN" => {
                let pattern = rest; // After "BAN <channel>" the rest is the pattern.
                self.cmd_ban(sender, arg1, pattern, true).await;
            }
            "UNBAN" => {
                let pattern = rest;
                self.cmd_ban(sender, arg1, pattern, false).await;
            }
            "HELP" => self.cmd_help(sender),
            _ => {
                reply(
                    sender,
                    &format!("Unknown command: {command}. Use HELP for a list of commands."),
                );
            }
        }

        let _ = state; // Will be used for access checks in the future.
    }
}

impl ChanServ {
    // -- REGISTER <channel> [description] -----------------------------------

    async fn cmd_register(&self, sender: &ClientHandle, channel: Option<&str>, desc: Option<&str>) {
        let Some(channel) = channel else {
            reply(sender, "Usage: REGISTER <#channel> [description]");
            return;
        };

        if !channel.starts_with('#') && !channel.starts_with('&') {
            reply(sender, "Invalid channel name.");
            return;
        }

        let key = channel.to_ascii_lowercase();
        let mut channels = self.inner.channels.write().await;

        if channels.contains_key(&key) {
            reply(sender, "This channel is already registered.");
            return;
        }

        let reg = RegisteredChannel {
            name: channel.to_string(),
            founder: sender.info.nick.to_ascii_lowercase(),
            min_reputation: 0,
            bans: Vec::new(),
            description: desc.map(|s| s.to_string()),
        };

        channels.insert(key, reg);
        drop(channels);
        self.persist().await;

        reply(
            sender,
            &format!("Channel {channel} registered. You are the founder."),
        );
        info!(channel = %channel, founder = %sender.info.nick, "ChanServ: channel registered");
    }

    // -- INFO <channel> -----------------------------------------------------

    async fn cmd_info(&self, sender: &ClientHandle, channel: Option<&str>) {
        let Some(channel) = channel else {
            reply(sender, "Usage: INFO <#channel>");
            return;
        };

        let key = channel.to_ascii_lowercase();
        let channels = self.inner.channels.read().await;

        match channels.get(&key) {
            None => reply(sender, &format!("{channel} is not registered.")),
            Some(reg) => {
                reply(sender, &format!("Information for \x02{}\x02:", reg.name));
                reply(sender, &format!("  Founder:         {}", reg.founder));
                reply(
                    sender,
                    &format!("  Min reputation:  {}", reg.min_reputation),
                );
                reply(sender, &format!("  Bans:            {}", reg.bans.len()));
                if let Some(ref desc) = reg.description {
                    reply(sender, &format!("  Description:     {desc}"));
                }
            }
        }
    }

    // -- SET <channel> <key> <value> ----------------------------------------

    async fn cmd_set(
        &self,
        sender: &ClientHandle,
        channel: Option<&str>,
        key: Option<&str>,
        value: Option<&str>,
    ) {
        let Some(channel) = channel else {
            reply(sender, "Usage: SET <#channel> <key> <value>");
            return;
        };
        let Some(key) = key else {
            reply(sender, "Available settings: MIN-REPUTATION, DESCRIPTION");
            return;
        };

        let chan_key = channel.to_ascii_lowercase();

        // Only founder can modify.
        if !self.is_founder(channel, &sender.info.nick).await {
            reply(sender, "You are not the founder of this channel.");
            return;
        }

        let mut channels = self.inner.channels.write().await;
        let Some(reg) = channels.get_mut(&chan_key) else {
            reply(sender, "This channel is not registered.");
            return;
        };

        match key.to_ascii_uppercase().as_str() {
            "MIN-REPUTATION" | "MINREP" => {
                let Some(val) = value.and_then(|v| v.parse::<i64>().ok()) else {
                    reply(sender, "Usage: SET <#channel> MIN-REPUTATION <number>");
                    return;
                };
                reg.min_reputation = val;
                drop(channels);
                self.persist().await;
                reply(
                    sender,
                    &format!("Minimum reputation for {channel} set to {val}."),
                );
            }
            "DESCRIPTION" | "DESC" => {
                reg.description = value.map(|s| s.to_string());
                drop(channels);
                self.persist().await;
                reply(sender, "Channel description updated.");
            }
            _ => {
                reply(
                    sender,
                    "Unknown setting. Available: MIN-REPUTATION, DESCRIPTION",
                );
            }
        }
    }

    // -- BAN/UNBAN <channel> <nick-pattern> ---------------------------------

    async fn cmd_ban(
        &self,
        sender: &ClientHandle,
        channel: Option<&str>,
        pattern: Option<&str>,
        add: bool,
    ) {
        let Some(channel) = channel else {
            let cmd = if add { "BAN" } else { "UNBAN" };
            reply(sender, &format!("Usage: {cmd} <#channel> <nick-pattern>"));
            return;
        };
        let Some(pattern) = pattern else {
            let cmd = if add { "BAN" } else { "UNBAN" };
            reply(sender, &format!("Usage: {cmd} <#channel> <nick-pattern>"));
            return;
        };

        if !self.is_founder(channel, &sender.info.nick).await {
            reply(sender, "You are not the founder of this channel.");
            return;
        }

        let chan_key = channel.to_ascii_lowercase();
        let pattern_lower = pattern.to_ascii_lowercase();

        let mut channels = self.inner.channels.write().await;
        let Some(reg) = channels.get_mut(&chan_key) else {
            reply(sender, "This channel is not registered.");
            return;
        };

        if add {
            if !reg.bans.contains(&pattern_lower) {
                reg.bans.push(pattern_lower.clone());
            }
            drop(channels);
            self.persist().await;
            reply(sender, &format!("Banned \x02{pattern}\x02 from {channel}."));
        } else {
            reg.bans.retain(|b| *b != pattern_lower);
            drop(channels);
            self.persist().await;
            reply(
                sender,
                &format!("Unbanned \x02{pattern}\x02 from {channel}."),
            );
        }
    }

    // -- HELP ---------------------------------------------------------------

    fn cmd_help(&self, sender: &ClientHandle) {
        let lines = [
            "ChanServ commands:",
            "  REGISTER <#channel> [desc]     — Register a channel (you become founder)",
            "  INFO <#channel>                — View channel registration info",
            "  SET <#channel> <key> <value>   — Change channel settings",
            "    Settings: MIN-REPUTATION, DESCRIPTION",
            "  BAN <#channel> <nick-pattern>  — Ban a nick pattern from joining",
            "  UNBAN <#channel> <nick-pattern> — Remove a ban",
            "  HELP                            — Show this help",
        ];
        for line in &lines {
            reply(sender, line);
        }
    }

    // -- Persistence --------------------------------------------------------

    async fn persist(&self) {
        let channels = self.inner.channels.read().await;
        if let Err(e) = save_channels(&channels) {
            warn!(error = %e, "ChanServ: failed to persist channels");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn reply(client: &ClientHandle, text: &str) {
    let msg = IrcMessage::notice(&client.info.nick, text).with_prefix(CHANSERV);
    client.send_message(&msg);
}

/// Simple glob matching: `*` matches any sequence of characters.
fn glob_match(pattern: &str, text: &str) -> bool {
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

fn load_channels() -> Result<HashMap<String, RegisteredChannel>, Box<dyn std::error::Error>> {
    let data = std::fs::read_to_string(PERSISTENCE_FILE)?;
    let map: HashMap<String, RegisteredChannel> = serde_json::from_str(&data)?;
    info!(count = map.len(), "ChanServ: loaded channels");
    Ok(map)
}

fn save_channels(
    channels: &HashMap<String, RegisteredChannel>,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = serde_json::to_string_pretty(channels)?;
    std::fs::write(PERSISTENCE_FILE, data)?;
    debug!(count = channels.len(), "ChanServ: persisted channels");
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
