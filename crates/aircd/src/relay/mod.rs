//! Relay abstraction for multi-instance aircd communication.
//!
//! The relay layer lets multiple aircd nodes share state (nick presence,
//! channel messages, DMs) over an inter-node transport.  The transport is
//! pluggable via the [`Relay`] trait:
//!
//! - [`NoopRelay`] — single-instance mode, zero overhead.
//! - (future) `RedisRelay` — Redis pub/sub backend behind a feature flag.
//!
//! # Design principles
//!
//! - The relay is a **dumb broadcast bus** for IRC messages. It has no
//!   knowledge of IRC routing (channels vs nicks, nick claiming, etc.).
//! - Each node publishes the IRC messages it processes locally. Every
//!   other node receives them and updates its own state accordingly.
//! - Nick presence is derived from the NICK/QUIT messages flowing
//!   through the bus — there is no separate claim/release protocol.
//! - Node lifecycle events (`NodeUp`/`NodeDown`) are surfaced by the
//!   backend (e.g. Redis heartbeat loss), not by the IRC message flow.
//! - No state is stored in the backend — each node maintains a local
//!   `nick_to_kind` cache populated from relayed messages.

mod noop;

pub use noop::NoopRelay;

use std::future::Future;
use std::pin::Pin;

use airc_shared::IrcMessage;
use tokio::sync::mpsc;

use crate::client::NodeId;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during relay operations.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Variants used by future relay backends (e.g. RedisRelay).
pub enum RelayError {
    #[error("relay transport error: {0}")]
    Transport(String),
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A message received from a remote node.
#[derive(Debug, Clone)]
pub struct RelayedMessage {
    /// The node that originated this message.
    pub source_node: NodeId,
    /// The IRC message to deliver/process locally.
    pub message: IrcMessage,
}

/// Events received from the relay transport.
#[derive(Debug)]
#[allow(dead_code)] // Variants constructed by future relay backends, dispatched in server.rs.
pub enum InboundEvent {
    /// An IRC message relayed from another node.
    Message(RelayedMessage),
    /// A remote node came online — here are the nicks it owns.
    NodeUp { node_id: NodeId, nicks: Vec<String> },
    /// A remote node went offline — all its nicks should be removed.
    NodeDown { node_id: NodeId },
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
/// The relay is a simple broadcast bus: nodes publish IRC messages, and
/// all other nodes receive them. The receiving node's inbound handler
/// (in `server.rs`) inspects each message and decides how to update
/// local state and notify local clients.
///
/// Uses boxed futures so the trait is object-safe — allowing
/// `Arc<dyn Relay>` in `SharedState`.
#[allow(dead_code)] // All methods used by future relay backends (e.g. RedisRelay).
pub trait Relay: Send + Sync + 'static {
    /// This node's unique identifier (generated once at startup).
    fn node_id(&self) -> &NodeId;

    /// Broadcast an IRC message to all other nodes.
    ///
    /// Called after a locally-processed command that mutates shared state
    /// or needs remote delivery (JOIN, PART, QUIT, NICK, PRIVMSG, etc.).
    /// Read-only queries (WHO, WHOIS, LIST, PING) are not published.
    fn publish(&self, message: &IrcMessage) -> BoxFuture<'_, Result<(), RelayError>>;

    /// Subscribe to inbound events from remote nodes.
    ///
    /// Returns a receiver that yields [`InboundEvent`]s. The caller (the
    /// relay task in `server.rs`) drives this receiver and applies events
    /// to `SharedState`.
    fn subscribe(&self) -> BoxFuture<'_, Result<mpsc::Receiver<InboundEvent>, RelayError>>;
}
