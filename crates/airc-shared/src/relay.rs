//! Relay wire format constants and helpers for the Redis pub/sub relay.
//!
//! The wire format is **binary protobuf**: every message published to
//! `airc:relay` is a `RelayEnvelope` encoded with
//! `prost::Message::encode_to_vec()` and decoded with
//! `prost::Message::decode()`.
//!
//! The protobuf types live in [`crate::relay_proto`] (generated from
//! `proto/relay.proto`). This module re-exports the most commonly used
//! types and provides the Redis channel name constants.

pub use crate::relay_proto::{
    AntiEntropyRequest, AntiEntropyResponse, ClientDown, ClientIntro, CrdtDelta, Join, Kick, Mode,
    NickChange, NodeDown, NodeUp, Notice, Part, Privmsg, Quit, RelayEnvelope, Topic,
    relay_envelope::Event as RelayEvent,
};

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// The Redis pub/sub channel all aircd nodes publish to and subscribe from.
pub const RELAY_CHANNEL: &str = "airc:relay";

/// Prefix for per-node heartbeat keys: `airc:heartbeat:<node_id>`.
pub const HEARTBEAT_KEY_PREFIX: &str = "airc:heartbeat:";
