//! AIRC IRC client library.
//!
//! Provides a high-level async IRC client that handles connection management,
//! registration handshake, automatic PING/PONG, and message buffering. Designed
//! to be used by both the `airc` daemon and future MCP server.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────┐       ┌────────────┐       ┌────────────┐
//! │ Caller   │─cmds─▶│ IrcClient  │─TCP──▶│ IRC Server │
//! │          │◀─recv─│ (tokio bg) │◀─TCP──│            │
//! └──────────┘       └────────────┘       └────────────┘
//! ```
//!
//! The [`IrcClient`] spawns background tasks for reading and writing. The
//! caller interacts via async methods that return immediately. Incoming
//! messages are buffered per-channel and can be drained on demand.
//!
//! # Example
//!
//! ```no_run
//! use airc_client::{IrcClient, ClientConfig};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = ClientConfig::new("irc.example.com:6667", "mybot");
//! let (client, _motd, _events) = IrcClient::connect(config).await?;
//! client.join("#lobby").await?;
//! client.say("#lobby", "Hello from an agent!").await?;
//! let msgs = client.fetch("#lobby").await;
//! client.quit(None).await?;
//! # Ok(())
//! # }
//! ```

mod client;
mod config;
mod conn;
mod error;
mod event;
mod state;

pub use client::IrcClient;
pub use config::{ClientConfig, DEFAULT_NICK, DEFAULT_SERVER, TlsMode};
pub use error::ClientError;
pub use event::{ChannelMessage, IrcEvent, MessageKind, new_channel_message};
pub use state::{ChannelStatus, ClientState};
