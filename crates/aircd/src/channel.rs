//! IRC channel state and operations.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::ClientId;

// ---------------------------------------------------------------------------
// Channel modes
// ---------------------------------------------------------------------------

/// Per-channel mode flags.
#[derive(Debug, Clone, Default)]
pub struct ChannelModes {
    pub invite_only: bool,
    pub topic_locked: bool,
    pub no_external: bool,
    pub moderated: bool,
    pub secret: bool,
    pub key: Option<String>,
    pub limit: Option<usize>,
}

impl ChannelModes {
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
// Member mode
// ---------------------------------------------------------------------------

/// The privilege level a client holds in a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemberMode {
    #[default]
    Normal,
    /// `+v` — voiced; can speak in moderated (+m) channels.
    Voice,
    /// `+o` — channel operator.
    Op,
}

impl MemberMode {
    /// IRC prefix character (`@` for op, `+` for voice, empty for normal).
    pub fn prefix(self) -> &'static str {
        match self {
            MemberMode::Op => "@",
            MemberMode::Voice => "+",
            MemberMode::Normal => "",
        }
    }

    /// All IRC prefix characters for multi-prefix support.
    ///
    /// With the current single-level model this is the same as `prefix()`,
    /// but the method name makes the intent clear at call sites.
    pub fn multi_prefix(self) -> &'static str {
        // With a single-level privilege model the multi-prefix string is
        // identical to the single prefix.  If a dual Op+Voice mode is ever
        // added this method should return "@+" for that case.
        self.prefix()
    }

    pub fn is_op(self) -> bool {
        self == MemberMode::Op
    }
    pub fn is_voice(self) -> bool {
        self == MemberMode::Voice
    }
}

// ---------------------------------------------------------------------------
// Membership
// ---------------------------------------------------------------------------

/// A single client's membership record in a channel.
#[derive(Debug, Clone)]
pub struct Membership {
    pub client_id: ClientId,
    pub mode: MemberMode,
    #[allow(dead_code)]
    pub joined_at: u64,
}

impl Membership {
    fn new(client_id: ClientId) -> Self {
        let joined_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            client_id,
            mode: MemberMode::Normal,
            joined_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// A single IRC channel.
#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub topic: Option<(String, String, u64)>,
    /// All members keyed by ClientId; includes mode and join timestamp.
    pub members: HashMap<ClientId, Membership>,
    pub modes: ChannelModes,
    /// Pre-join invite list (for +i enforcement).
    pub invited: HashSet<ClientId>,
    pub created_at: u64,
}

/// A lightweight snapshot of a channel's read-only state, excluding the
/// `members` HashMap (which can hold tens of thousands of entries).
///
/// Use this instead of a full `Channel` clone whenever the caller only needs
/// mode flags, topic, or invite information — not per-member queries.
/// Per-member queries (`is_member`, `is_operator`, `is_voiced`) should use
/// the targeted `SharedState::is_channel_member_id` / `*_operator_id` /
/// `*_voiced_id` helpers that acquire the channel lock only for that lookup.
#[derive(Debug, Clone)]
pub struct ChannelView {
    #[allow(dead_code)]
    pub name: String,
    pub topic: Option<(String, String, u64)>,
    pub modes: ChannelModes,
    pub invited: HashSet<ClientId>,
    #[allow(dead_code)]
    pub created_at: u64,
    pub member_count: usize,
}

impl ChannelView {
    /// Build a `ChannelView` from a channel guard without cloning `members`.
    pub fn from_channel(ch: &Channel) -> Self {
        Self {
            name: ch.name.clone(),
            topic: ch.topic.clone(),
            modes: ch.modes.clone(),
            invited: ch.invited.clone(),
            created_at: ch.created_at,
            member_count: ch.member_count(),
        }
    }

    pub fn is_invited(&self, id: ClientId) -> bool {
        self.invited.contains(&id)
    }
}

impl Channel {
    pub fn new(name: String) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            name,
            topic: None,
            members: HashMap::new(),
            modes: ChannelModes {
                no_external: true,
                topic_locked: true,
                ..Default::default()
            },
            invited: HashSet::new(),
            created_at,
        }
    }

    /// Add a member with Normal mode. Returns `true` if newly inserted.
    pub fn add_member(&mut self, id: ClientId) -> bool {
        if self.members.contains_key(&id) {
            return false;
        }
        self.members.insert(id, Membership::new(id));
        true
    }

    /// Remove a member. Returns `true` if they were present.
    pub fn remove_member_by_id(&mut self, id: ClientId) -> bool {
        self.members.remove(&id).is_some()
    }

    pub fn is_member(&self, id: ClientId) -> bool {
        self.members.contains_key(&id)
    }

    pub fn is_operator(&self, id: ClientId) -> bool {
        self.members.get(&id).is_some_and(|m| m.mode.is_op())
    }

    pub fn is_voiced(&self, id: ClientId) -> bool {
        self.members.get(&id).is_some_and(|m| m.mode.is_voice())
    }

    pub fn set_operator(&mut self, id: ClientId, grant: bool) -> bool {
        if let Some(m) = self.members.get_mut(&id) {
            m.mode = if grant {
                MemberMode::Op
            } else {
                MemberMode::Normal
            };
            true
        } else {
            false
        }
    }

    pub fn set_voice(&mut self, id: ClientId, grant: bool) -> bool {
        if let Some(m) = self.members.get_mut(&id) {
            m.mode = if grant {
                MemberMode::Voice
            } else {
                MemberMode::Normal
            };
            true
        } else {
            false
        }
    }

    pub fn set_topic(&mut self, text: String, setter: String, timestamp: u64) {
        self.topic = Some((text, setter, timestamp));
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }

    pub fn all_member_ids(&self) -> Vec<ClientId> {
        self.members.keys().copied().collect()
    }

    pub fn add_invite(&mut self, id: ClientId) {
        self.invited.insert(id);
    }
    pub fn is_invited(&self, id: ClientId) -> bool {
        self.invited.contains(&id)
    }
    pub fn clear_invite(&mut self, id: ClientId) {
        self.invited.remove(&id);
    }
}
