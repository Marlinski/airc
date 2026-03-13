//! IRC events — typed representations of incoming messages.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// Re-export proto types as the canonical message types.
pub use airc_shared::common::ChannelMessage;
pub use airc_shared::common::MessageKind;

/// A high-level IRC event parsed from the wire.
///
/// This is an internal client event type — NOT a protobuf type. It carries
/// richer variant data (Registered, Raw, etc.) that are only meaningful
/// inside the client library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IrcEvent {
    /// Successfully registered with the server.
    Registered {
        nick: String,
        server: String,
        message: String,
    },
    /// A message in a channel or private message.
    Message(ChannelMessage),
    /// Someone joined a channel.
    Join { nick: String, channel: String },
    /// Someone left a channel.
    Part {
        nick: String,
        channel: String,
        reason: Option<String>,
    },
    /// Someone quit IRC.
    Quit {
        nick: String,
        reason: Option<String>,
    },
    /// Someone was kicked from a channel.
    Kick {
        channel: String,
        nick: String,
        by: String,
        reason: Option<String>,
    },
    /// Channel topic changed.
    TopicChange {
        channel: String,
        topic: String,
        set_by: String,
    },
    /// Nick change.
    NickChange { old_nick: String, new_nick: String },
    /// A notice (from server or user).
    Notice {
        from: Option<String>,
        target: String,
        text: String,
    },
    /// Connection was lost (will attempt reconnect if auto-reconnect is enabled).
    Disconnected { reason: String },
    /// A reconnection attempt is starting.
    Reconnecting { attempt: u32 },
    /// Successfully reconnected after a disconnection.
    Reconnected,
    /// An unhandled/raw IRC message.
    Raw { line: String },
}

/// Create a new `ChannelMessage` with the current timestamp.
pub fn new_channel_message(
    target: String,
    from: String,
    text: String,
    kind: MessageKind,
) -> ChannelMessage {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    ChannelMessage {
        target,
        from,
        text,
        kind: kind as i32,
        timestamp,
    }
}
