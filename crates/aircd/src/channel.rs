//! IRC channel state and operations.

use std::collections::{HashMap, HashSet};

use crate::client::{ClientId, ClientKind, NodeId};

// ---------------------------------------------------------------------------
// Channel modes
// ---------------------------------------------------------------------------

/// Per-channel mode flags.
#[derive(Debug, Clone, Default)]
pub struct ChannelModes {
    /// `+i` — invite-only.
    pub invite_only: bool,
    /// `+t` — only operators may set the topic.
    pub topic_locked: bool,
    /// `+n` — no external messages (must be a member to send).
    pub no_external: bool,
    /// `+m` — moderated (only voiced/opped users can speak).
    pub moderated: bool,
    /// `+s` — secret (channel hidden from LIST/WHOIS for non-members).
    pub secret: bool,
    /// `+k` — channel key (password).
    pub key: Option<String>,
    /// `+l` — member limit.
    pub limit: Option<usize>,
}

impl ChannelModes {
    /// Render the current mode string (e.g. `+intms`).
    pub fn to_mode_string(&self) -> String {
        let mut s = String::from("+");
        if self.invite_only {
            s.push('i');
        }
        if self.moderated {
            s.push('m');
        }
        if self.no_external {
            s.push('n');
        }
        if self.secret {
            s.push('s');
        }
        if self.topic_locked {
            s.push('t');
        }
        if self.key.is_some() {
            s.push('k');
        }
        if self.limit.is_some() {
            s.push('l');
        }
        if s.len() == 1 { "+".to_string() } else { s }
    }
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// A single IRC channel.
///
/// Members are keyed by lowercase nick and carry a [`ClientKind`] that tells
/// us whether the user is local (has a `ClientId` → `ClientHandle`) or remote
/// (reachable via relay to a `NodeId`).
#[derive(Debug, Clone)]
pub struct Channel {
    /// Canonical channel name (preserves original casing).
    pub name: String,
    /// Current topic: `(text, setter_nick, unix_timestamp)`.
    pub topic: Option<(String, String, u64)>,
    /// All members: lowercase nick → local or remote.
    pub members: HashMap<String, ClientKind>,
    /// Operators: lowercase nicks (works uniformly for local and remote).
    pub operators: HashSet<String>,
    /// Voiced users: lowercase nicks (can speak in +m channels).
    pub voiced: HashSet<String>,
    /// Channel mode flags.
    pub modes: ChannelModes,
    /// Nicks that have been invited to this channel (for `+i` enforcement).
    /// Stored as lowercase nicks so lookup is case-insensitive.
    pub invited: HashSet<String>,
    /// Unix timestamp when the channel was created.
    pub created_at: u64,
}

impl Channel {
    /// Create a new, empty channel with default modes (`+nt`).
    pub fn new(name: String) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            name,
            topic: None,
            members: HashMap::new(),
            operators: HashSet::new(),
            voiced: HashSet::new(),
            modes: ChannelModes {
                no_external: true,
                topic_locked: true,
                ..Default::default()
            },
            invited: HashSet::new(),
            created_at,
        }
    }

    /// Add a member by nick. Returns `true` if the member was newly inserted.
    pub fn add_member(&mut self, nick: &str, kind: ClientKind) -> bool {
        let nick_lower = nick.to_ascii_lowercase();
        if self.members.contains_key(&nick_lower) {
            return false;
        }
        self.members.insert(nick_lower, kind);
        true
    }

    /// Remove a member by nick (also strips operator/voice status). Returns `true` if present.
    pub fn remove_member(&mut self, nick: &str) -> bool {
        let nick_lower = nick.to_ascii_lowercase();
        self.operators.remove(&nick_lower);
        self.voiced.remove(&nick_lower);
        self.members.remove(&nick_lower).is_some()
    }

    /// Remove a member by `ClientId` (for local client cleanup).
    /// Returns the nick if found and removed.
    pub fn remove_member_by_id(&mut self, id: ClientId) -> Option<String> {
        let nick = self
            .members
            .iter()
            .find(|(_, kind)| matches!(kind, ClientKind::Local(cid) if *cid == id))
            .map(|(nick, _)| nick.clone());
        if let Some(ref nick) = nick {
            self.operators.remove(nick);
            self.voiced.remove(nick);
            self.members.remove(nick);
        }
        nick
    }

    /// Whether a nick is a member of this channel.
    pub fn is_member_nick(&self, nick: &str) -> bool {
        self.members.contains_key(&nick.to_ascii_lowercase())
    }

    /// Whether a `ClientId` is a member of this channel.
    pub fn is_member_id(&self, id: ClientId) -> bool {
        self.members
            .values()
            .any(|kind| matches!(kind, ClientKind::Local(cid) if *cid == id))
    }

    /// Whether a nick is an operator in this channel.
    #[allow(dead_code)] // Used when relay is wired up.
    pub fn is_operator(&self, nick: &str) -> bool {
        self.operators.contains(&nick.to_ascii_lowercase())
    }

    /// Whether a nick is voiced in this channel.
    #[allow(dead_code)] // Available for direct queries.
    pub fn is_voiced(&self, nick: &str) -> bool {
        self.voiced.contains(&nick.to_ascii_lowercase())
    }

    /// Whether a nick can speak in this channel (operator, voiced, or channel is not moderated).
    pub fn can_speak(&self, nick: &str) -> bool {
        if !self.modes.moderated {
            return true;
        }
        let nick_lower = nick.to_ascii_lowercase();
        self.operators.contains(&nick_lower) || self.voiced.contains(&nick_lower)
    }

    /// Whether a `ClientId` is an operator in this channel.
    pub fn is_operator_id(&self, id: ClientId) -> bool {
        // Find the nick for this ClientId, then check operators.
        self.members.iter().any(|(nick, kind)| {
            matches!(kind, ClientKind::Local(cid) if *cid == id) && self.operators.contains(nick)
        })
    }

    /// Set the channel topic.
    pub fn set_topic(&mut self, text: String, setter: String, timestamp: u64) {
        self.topic = Some((text, setter, timestamp));
    }

    /// Number of members (local + remote).
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// All local `ClientId`s in this channel.
    pub fn local_client_ids(&self) -> Vec<ClientId> {
        self.members
            .values()
            .filter_map(|kind| match kind {
                ClientKind::Local(id) => Some(*id),
                ClientKind::Remote(_) => None,
            })
            .collect()
    }

    /// All unique remote `NodeId`s that have members in this channel.
    #[allow(dead_code)] // Used when relay is wired up.
    pub fn remote_node_ids(&self) -> HashSet<&NodeId> {
        self.members
            .values()
            .filter_map(|kind| match kind {
                ClientKind::Remote(node_id) => Some(node_id),
                ClientKind::Local(_) => None,
            })
            .collect()
    }

    /// Snapshot of all member nicks.
    #[allow(dead_code)] // Used when relay is wired up.
    pub fn member_nicks(&self) -> Vec<String> {
        self.members.keys().cloned().collect()
    }

    /// Snapshot of member nicks with status prefix (`@` for ops, `+` for voiced).
    pub fn nicks_with_prefix(&self) -> Vec<String> {
        self.members
            .keys()
            .map(|nick| {
                let prefix = if self.operators.contains(nick) {
                    "@"
                } else if self.voiced.contains(nick) {
                    "+"
                } else {
                    ""
                };
                format!("{prefix}{nick}")
            })
            .collect()
    }

    /// Find the nick (lowercase) for a given `ClientId`, if they are a local member.
    pub fn nick_for_id(&self, id: ClientId) -> Option<&str> {
        self.members
            .iter()
            .find(|(_, kind)| matches!(kind, ClientKind::Local(cid) if *cid == id))
            .map(|(nick, _)| nick.as_str())
    }

    /// Add a nick to the invite list (case-insensitive).
    pub fn add_invite(&mut self, nick: &str) {
        self.invited.insert(nick.to_ascii_lowercase());
    }

    /// Check whether a nick has been invited (case-insensitive).
    pub fn is_invited(&self, nick: &str) -> bool {
        self.invited.contains(&nick.to_ascii_lowercase())
    }

    /// Remove a nick from the invite list after they join (case-insensitive).
    pub fn clear_invite(&mut self, nick: &str) {
        self.invited.remove(&nick.to_ascii_lowercase());
    }

    /// Remove all members belonging to a specific remote node.
    /// Returns the list of removed nicks.
    #[allow(dead_code)] // Used when relay is wired up.
    pub fn remove_node_members(&mut self, node_id: &NodeId) -> Vec<String> {
        let to_remove: Vec<String> = self
            .members
            .iter()
            .filter(|(_, kind)| matches!(kind, ClientKind::Remote(nid) if nid == node_id))
            .map(|(nick, _)| nick.clone())
            .collect();
        for nick in &to_remove {
            self.operators.remove(nick);
            self.voiced.remove(nick);
            self.members.remove(nick);
        }
        to_remove
    }
}
