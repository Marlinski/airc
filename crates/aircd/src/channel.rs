//! IRC channel state and operations.

use std::collections::HashSet;

use crate::client::ClientId;

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
    /// `+k` — channel key (password).
    pub key: Option<String>,
    /// `+l` — member limit.
    pub limit: Option<usize>,
}

impl ChannelModes {
    /// Render the current mode string (e.g. `+int`).
    pub fn to_mode_string(&self) -> String {
        let mut s = String::from("+");
        if self.invite_only {
            s.push('i');
        }
        if self.no_external {
            s.push('n');
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
#[derive(Debug, Clone)]
pub struct Channel {
    /// Canonical channel name (preserves original casing).
    pub name: String,
    /// Current topic: `(text, setter_nick, unix_timestamp)`.
    pub topic: Option<(String, String, u64)>,
    /// Set of all members currently in the channel.
    pub members: HashSet<ClientId>,
    /// Subset of members who have operator (`+o`) status.
    pub operators: HashSet<ClientId>,
    /// Channel mode flags.
    pub modes: ChannelModes,
}

impl Channel {
    /// Create a new, empty channel with default modes (`+nt`).
    pub fn new(name: String) -> Self {
        Self {
            name,
            topic: None,
            members: HashSet::new(),
            operators: HashSet::new(),
            modes: ChannelModes {
                no_external: true,
                topic_locked: true,
                ..Default::default()
            },
        }
    }

    /// Add a member. Returns `true` if the member was newly inserted.
    pub fn add_member(&mut self, id: ClientId) -> bool {
        self.members.insert(id)
    }

    /// Remove a member (also strips operator status). Returns `true` if present.
    pub fn remove_member(&mut self, id: ClientId) -> bool {
        self.operators.remove(&id);
        self.members.remove(&id)
    }

    /// Whether `id` is a member of this channel.
    pub fn is_member(&self, id: ClientId) -> bool {
        self.members.contains(&id)
    }

    /// Whether `id` is an operator in this channel.
    pub fn is_operator(&self, id: ClientId) -> bool {
        self.operators.contains(&id)
    }

    /// Set the channel topic.
    pub fn set_topic(&mut self, text: String, setter: String, timestamp: u64) {
        self.topic = Some((text, setter, timestamp));
    }

    /// Number of members.
    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    /// Snapshot of member IDs.
    pub fn member_list(&self) -> Vec<ClientId> {
        self.members.iter().copied().collect()
    }
}
