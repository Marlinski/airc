//! IRC protocol message parsing and shared protobuf data models.
//!
//! This crate provides:
//!
//! - [`IrcMessage`] — parse and serialize IRC wire protocol messages (RFC 2812)
//! - [`Command`] — typed IRC command enum
//! - [`reply`] — numeric reply code constants and helpers
//! - [`prefix`] — structured message prefix (nick!user@host)
//! - [`validate`] — protocol-level validation (nicknames, channel names)
//! - [`log`] — CSV log file support (serialization, `FileLogger`)
//! - [`common`] — shared protobuf-generated data models (messages, events)
//! - [`ipc`] — CLI-to-daemon IPC protobuf types
//! - [`aird_ipc`] — daemon controller IPC protobuf types
//! - [`http_api`] — HTTP API protobuf types
//!
//! # Protobuf modules
//!
//! The `common`, `ipc`, `aird_ipc`, and `http_api` modules are generated from
//! `.proto` files in the `proto/` directory by `prost-build`. All generated
//! types have serde derives for JSON serialization and `prost::Message` for
//! binary protobuf encoding.
//!
//! # Examples
//!
//! ```
//! use airc_shared::{IrcMessage, Command};
//!
//! // Parse a raw IRC line
//! let msg = IrcMessage::parse(":nick!user@host PRIVMSG #channel :hello").unwrap();
//! assert_eq!(msg.command, Command::Privmsg);
//! assert_eq!(msg.params[1], "hello");
//!
//! // Construct and serialize
//! let reply = IrcMessage::privmsg("#channel", "hi back")
//!     .with_prefix("server");
//! assert_eq!(reply.serialize(), ":server PRIVMSG #channel :hi back");
//! ```

pub mod log;
pub mod message;
pub mod prefix;
pub mod reply;
pub mod validate;

// Protobuf-generated modules.
pub mod common {
    include!(concat!(env!("OUT_DIR"), "/airc.common.rs"));
}

pub mod ipc {
    include!(concat!(env!("OUT_DIR"), "/airc.ipc.rs"));
}

pub mod aird_ipc {
    include!(concat!(env!("OUT_DIR"), "/airc.aird_ipc.rs"));
}

pub mod http_api {
    include!(concat!(env!("OUT_DIR"), "/airc.http_api.rs"));
}

// Re-export core IRC protocol types.
pub use message::{Command, IrcMessage, ParseError};
pub use prefix::Prefix;

// Re-export commonly used protobuf types at the crate root for convenience.
pub use common::{ChannelMessage, ChannelStatus, EventType, LogEvent, MessageKind};
