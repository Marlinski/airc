//! Relay abstraction for multi-instance aircd communication.
//!
//! The relay layer lets multiple aircd nodes share state (nick presence,
//! channel messages, DMs) over an inter-node transport.  The transport is
//! pluggable via the [`Relay`] trait:
//!
//! - [`NoopRelay`] — single-instance mode, zero overhead.
//! - [`RedisRelay`] — Redis pub/sub backend behind a feature flag.
//!
//! # Design principles
//!
//! - The relay is a **typed broadcast bus**. Every inter-node event is a
//!   structured protobuf message (defined in `proto/relay.proto`), not a raw
//!   IRC wire string.
//! - Each node publishes the events it processes locally. Every other node
//!   receives them and updates its own state accordingly.
//! - Client identity is stable via `ClientId` — nick changes never require
//!   updating channel membership structures.
//! - Node lifecycle events (`NodeUp`/`NodeDown`) are surfaced by the backend
//!   (e.g. Redis heartbeat loss), not by the IRC message flow.
//! - No state is stored in the backend — each node maintains a local `users`
//!   registry populated from relayed `ClientIntro` events.

mod noop;
mod redis;

pub use noop::NoopRelay;
pub use redis::RedisRelay;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;

use crate::channel::{ChannelModes, MemberMode};
use crate::client::{Client, ClientId, ClientInfo, NodeId};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during relay operations.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Variants used by relay backends.
pub enum RelayError {
    #[error("relay transport error: {0}")]
    Transport(String),
}

// ---------------------------------------------------------------------------
// Unified relay event
// ---------------------------------------------------------------------------

/// Unified event type used for both publishing and subscribing on the relay.
///
/// Outbound events (published by local handlers) and inbound events (received
/// from remote nodes) share this single enum, eliminating the need for a
/// separate `InboundEvent` type.
#[derive(Debug)]
#[allow(dead_code)] // Variants dispatched in server.rs.
pub enum RelayEvent {
    /// A new client has registered on another node.
    ///
    /// `node_id` is the ID of the node that owns this client.  Receivers must
    /// use this to register the client as `Remote`, regardless of the
    /// `ClientKind` carried in `client` (which may be `Local` when the event
    /// travels in-process, e.g. in test `PairRelay`).
    ClientIntro { client: Client, node_id: NodeId },
    /// A remote client has disconnected.
    ClientDown { client_id: ClientId },
    /// A client changed their nick.
    NickChange {
        client_id: ClientId,
        new_nick: String,
    },
    /// A client joined a channel.
    Join {
        client_id: ClientId,
        channel: String,
    },
    /// A client parted a channel.
    Part {
        client_id: ClientId,
        channel: String,
        reason: Option<String>,
    },
    /// A client quit.
    Quit {
        client_id: ClientId,
        reason: Option<String>,
    },
    /// A client sent a PRIVMSG.
    Privmsg {
        client_id: ClientId,
        target: String,
        text: String,
    },
    /// A client sent a NOTICE.
    Notice {
        client_id: ClientId,
        target: String,
        text: String,
    },
    /// A client set a topic.
    Topic {
        client_id: ClientId,
        channel: String,
        text: String,
    },
    /// A client applied a mode change.
    Mode {
        client_id: ClientId,
        target: String,
        mode_string: String,
    },
    /// A client kicked someone.
    Kick {
        client_id: ClientId,
        channel: String,
        target_client_id: ClientId,
        reason: String,
    },
    /// A CRDT delta (full blob) — apply and write-through.
    CrdtDelta {
        /// Logical CRDT identifier, e.g. `"ban:#channel"`, `"nick:alice"`.
        crdt_id: String,
        /// Bincode-serialised CRDT blob. Idempotent: merging twice is safe.
        payload: Vec<u8>,
    },
    /// Anti-entropy request from a node.
    AntiEntropyRequest {
        from_node: String,
        hashes: HashMap<String, Vec<u8>>,
    },
    /// Anti-entropy response: full CRDT blobs for every diverged CRDT.
    AntiEntropyResponse {
        from_node: String,
        blobs: HashMap<String, Vec<u8>>,
    },
    /// A remote node came online — flood it with our ClientIntro events.
    NodeUp { node_id: NodeId },
    /// A remote node went offline — remove all its clients.
    NodeDown { node_id: NodeId },
    /// Full running-state snapshot sent to a newly joined node.
    ///
    /// The receiving node checks `target_node_id` against its own node ID;
    /// if it matches, it applies the snapshot to populate its local running
    /// state.  All other nodes ignore it.
    StateSnapshot {
        /// Only the node with this ID should apply the snapshot.
        target_node_id: NodeId,
        /// All connected clients on the sending node.
        clients: Vec<SnapshotClient>,
        /// All channels on the sending node.
        channels: Vec<SnapshotChannel>,
        /// All memberships on the sending node.
        memberships: Vec<SnapshotMembership>,
    },
}

// ---------------------------------------------------------------------------
// StateSnapshot payload types
// ---------------------------------------------------------------------------

/// Compact representation of a single connected client for state snapshots.
#[derive(Debug)]
pub struct SnapshotClient {
    pub client_id: ClientId,
    pub info: ClientInfo,
    pub node_id: NodeId,
}

/// Compact representation of a channel for state snapshots.
#[derive(Debug)]
pub struct SnapshotChannel {
    /// Stable channel ID (fnv1a hash of lowercase name).
    pub channel_id: u64,
    pub name: String,
    pub topic: Option<(String, String, u64)>,
    pub modes: ChannelModes,
    pub created_at: u64,
}

/// A single membership record for state snapshots.
#[derive(Debug)]
pub struct SnapshotMembership {
    pub client_id: ClientId,
    pub channel_id: u64,
    pub mode: MemberMode,
}

// ---------------------------------------------------------------------------
// Convenience type alias for boxed futures (object-safe async)
// ---------------------------------------------------------------------------

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over the inter-node relay transport.
///
/// Uses boxed futures so the trait is object-safe — allowing `Arc<dyn Relay>`
/// in `SharedState`.
#[allow(dead_code)]
pub trait Relay: Send + Sync + 'static {
    /// This node's unique identifier (generated once at startup).
    fn node_id(&self) -> &NodeId;

    /// Publish a relay event to all remote nodes.
    fn publish(&self, event: RelayEvent) -> BoxFuture<'_, Result<(), RelayError>>;

    /// Subscribe to events from remote nodes.
    ///
    /// Returns a receiver that yields [`RelayEvent`]s. The caller (the relay
    /// task in `server.rs`) drives this receiver and applies events to
    /// `SharedState`.
    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<RelayEvent>, RelayError>>;
}
