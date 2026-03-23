//! Client-side state tracking.
//!
//! Tracks the client's own identity, joined channels, and buffered messages.
//! All state is behind a lock so it can be updated by the reader task and
//! queried by the caller.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use tokio::sync::RwLock;

use airc_shared::common::ChannelMessage;

// Re-export the proto ChannelStatus as the canonical type.
pub use airc_shared::common::ChannelStatus;

/// Shared client state, safe to clone and pass across tasks.
#[derive(Debug, Clone)]
pub struct ClientState {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Debug)]
struct Inner {
    /// Our current nick (may change if server forces it).
    nick: String,
    /// The server name (from RPL_WELCOME).
    server_name: Option<String>,
    /// Channels we've joined, mapped to their member lists.
    channels: HashMap<String, ChannelState>,
    /// Max messages to buffer per channel.
    buffer_size: usize,
    /// Whether we've completed registration.
    registered: bool,
    /// Whether SASL authentication completed successfully.
    ///
    /// Used by the registration handler to decide whether to fall back to
    /// NickServ IDENTIFY after RPL_WELCOME.
    sasl_logged_in: bool,
}

/// State for a single channel.
#[derive(Debug, Clone)]
pub struct ChannelState {
    /// Channel name.
    pub name: String,
    /// Current topic.
    pub topic: Option<String>,
    /// Known members (nicks).
    pub members: Vec<String>,
    /// Buffered messages (bounded ring).
    pub messages: VecDeque<ChannelMessage>,
    /// Read cursor — index of the next unread message.
    /// Messages before this index have been "fetched".
    pub read_cursor: usize,
}

impl ClientState {
    /// Create a new client state.
    pub fn new(nick: String, buffer_size: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                nick,
                server_name: None,
                channels: HashMap::new(),
                buffer_size,
                registered: false,
                sasl_logged_in: false,
            })),
        }
    }

    /// Get our current nick.
    pub async fn nick(&self) -> String {
        self.inner.read().await.nick.clone()
    }

    /// Set our nick (called when server confirms a nick change).
    pub async fn set_nick(&self, nick: String) {
        self.inner.write().await.nick = nick;
    }

    /// Get the server name.
    pub async fn server_name(&self) -> Option<String> {
        self.inner.read().await.server_name.clone()
    }

    /// Set the server name.
    pub async fn set_server_name(&self, name: String) {
        self.inner.write().await.server_name = Some(name);
    }

    /// Mark registration as complete.
    pub async fn set_registered(&self) {
        self.inner.write().await.registered = true;
    }

    /// Whether we're registered.
    pub async fn is_registered(&self) -> bool {
        self.inner.read().await.registered
    }

    /// Mark SASL authentication as successfully completed.
    pub async fn set_sasl_logged_in(&self) {
        self.inner.write().await.sasl_logged_in = true;
    }

    /// Whether SASL authentication completed successfully.
    pub async fn is_sasl_logged_in(&self) -> bool {
        self.inner.read().await.sasl_logged_in
    }

    /// Record that we joined a channel.
    pub async fn join_channel(&self, name: &str) {
        let mut inner = self.inner.write().await;
        let key = name.to_ascii_lowercase();
        inner.channels.entry(key).or_insert_with(|| ChannelState {
            name: name.to_string(),
            topic: None,
            members: Vec::new(),
            messages: VecDeque::new(),
            read_cursor: 0,
        });
    }

    /// Record that we left a channel.
    pub async fn part_channel(&self, name: &str) {
        let mut inner = self.inner.write().await;
        inner.channels.remove(&name.to_ascii_lowercase());
    }

    /// Get the list of channels we're in.
    pub async fn channels(&self) -> Vec<String> {
        let inner = self.inner.read().await;
        inner.channels.values().map(|c| c.name.clone()).collect()
    }

    /// Set the topic for a channel.
    pub async fn set_topic(&self, channel: &str, topic: String) {
        let mut inner = self.inner.write().await;
        if let Some(ch) = inner.channels.get_mut(&channel.to_ascii_lowercase()) {
            ch.topic = Some(topic);
        }
    }

    /// Set the member list for a channel (from NAMES reply).
    pub async fn set_members(&self, channel: &str, members: Vec<String>) {
        let mut inner = self.inner.write().await;
        if let Some(ch) = inner.channels.get_mut(&channel.to_ascii_lowercase()) {
            ch.members = members;
        }
    }

    /// Add a member to a channel.
    pub async fn add_member(&self, channel: &str, nick: &str) {
        let mut inner = self.inner.write().await;
        if let Some(ch) = inner.channels.get_mut(&channel.to_ascii_lowercase())
            && !ch.members.iter().any(|n| n.eq_ignore_ascii_case(nick))
        {
            ch.members.push(nick.to_string());
        }
    }

    /// Remove a member from a channel.
    pub async fn remove_member(&self, channel: &str, nick: &str) {
        let mut inner = self.inner.write().await;
        if let Some(ch) = inner.channels.get_mut(&channel.to_ascii_lowercase()) {
            ch.members.retain(|n| !n.eq_ignore_ascii_case(nick));
        }
    }

    /// Remove a member from all channels (e.g., on QUIT).
    pub async fn remove_member_all(&self, nick: &str) {
        let mut inner = self.inner.write().await;
        for ch in inner.channels.values_mut() {
            ch.members.retain(|n| !n.eq_ignore_ascii_case(nick));
        }
    }

    /// Buffer an incoming message for a channel.
    pub async fn push_message(&self, channel: &str, msg: ChannelMessage) {
        let mut inner = self.inner.write().await;
        let key = channel.to_ascii_lowercase();
        let buf_size = inner.buffer_size;
        if let Some(ch) = inner.channels.get_mut(&key) {
            ch.messages.push_back(msg);
            // Trim to buffer size.
            while ch.messages.len() > buf_size {
                ch.messages.pop_front();
                // Adjust cursor if it pointed to a removed message.
                if ch.read_cursor > 0 {
                    ch.read_cursor -= 1;
                }
            }
        }
    }

    /// Buffer a private message (not in a channel). Stored under the
    /// sender's nick as the "channel" key.
    pub async fn push_private_message(&self, msg: ChannelMessage) {
        let mut inner = self.inner.write().await;
        let key = msg.from.to_ascii_lowercase();
        let buf_size = inner.buffer_size;
        let ch = inner.channels.entry(key).or_insert_with(|| ChannelState {
            name: msg.from.clone(),
            topic: None,
            members: Vec::new(),
            messages: VecDeque::new(),
            read_cursor: 0,
        });
        ch.messages.push_back(msg);
        while ch.messages.len() > buf_size {
            ch.messages.pop_front();
            if ch.read_cursor > 0 {
                ch.read_cursor -= 1;
            }
        }
    }

    /// Fetch unread messages for a channel (advances the read cursor).
    pub async fn fetch(&self, channel: &str) -> Vec<ChannelMessage> {
        let mut inner = self.inner.write().await;
        let key = channel.to_ascii_lowercase();
        if let Some(ch) = inner.channels.get_mut(&key) {
            let unread: Vec<_> = ch.messages.iter().skip(ch.read_cursor).cloned().collect();
            ch.read_cursor = ch.messages.len();
            unread
        } else {
            Vec::new()
        }
    }

    /// Fetch unread messages from ALL channels.
    pub async fn fetch_all(&self) -> Vec<ChannelMessage> {
        let mut inner = self.inner.write().await;
        let mut all = Vec::new();
        for ch in inner.channels.values_mut() {
            let unread: Vec<_> = ch.messages.iter().skip(ch.read_cursor).cloned().collect();
            ch.read_cursor = ch.messages.len();
            all.extend(unread);
        }
        // Sort by timestamp.
        all.sort_by_key(|m| m.timestamp);
        all
    }

    /// Fetch the last N messages from a channel and mark all as read.
    ///
    /// Returns at most `n` messages from the tail of the buffer. The read
    /// cursor is advanced to the end regardless of how many messages are
    /// returned, so any earlier unread messages are implicitly consumed.
    pub async fn fetch_last(&self, channel: &str, n: usize) -> Vec<ChannelMessage> {
        let mut inner = self.inner.write().await;
        let key = channel.to_ascii_lowercase();
        if let Some(ch) = inner.channels.get_mut(&key) {
            let start = ch.messages.len().saturating_sub(n);
            let msgs = ch.messages.iter().skip(start).cloned().collect();
            ch.read_cursor = ch.messages.len();
            msgs
        } else {
            Vec::new()
        }
    }

    /// Fetch the last N messages from ALL channels and mark all as read.
    ///
    /// Messages are sorted by timestamp. The read cursor on every channel
    /// is advanced to the end.
    pub async fn fetch_last_all(&self, n: usize) -> Vec<ChannelMessage> {
        let mut inner = self.inner.write().await;
        let mut all = Vec::new();
        for ch in inner.channels.values_mut() {
            all.extend(ch.messages.iter().cloned());
            ch.read_cursor = ch.messages.len();
        }
        all.sort_by_key(|m| m.timestamp);
        let start = all.len().saturating_sub(n);
        all.split_off(start)
    }

    /// Get a summary of all channels: name, unread count, member count.
    pub async fn status(&self) -> Vec<ChannelStatus> {
        let inner = self.inner.read().await;
        inner
            .channels
            .values()
            .map(|ch| ChannelStatus {
                name: ch.name.clone(),
                topic: ch.topic.clone(),
                members: ch.members.len() as u32,
                total_messages: ch.messages.len() as u32,
                unread: ch.messages.len().saturating_sub(ch.read_cursor) as u32,
            })
            .collect()
    }

    /// Update nick in member lists when someone changes nick.
    pub async fn rename_member(&self, old_nick: &str, new_nick: &str) {
        let mut inner = self.inner.write().await;
        for ch in inner.channels.values_mut() {
            for member in ch.members.iter_mut() {
                if member.eq_ignore_ascii_case(old_nick) {
                    *member = new_nick.to_string();
                }
            }
        }
    }
}
