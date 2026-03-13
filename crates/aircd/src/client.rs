//! Per-client data and the handle used to communicate with a connected client.

use std::fmt;

use airc_shared::IrcMessage;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// ClientId
// ---------------------------------------------------------------------------

/// Unique, opaque identifier for a connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// ClientInfo
// ---------------------------------------------------------------------------

/// Stored data about a client — identity, registration status, modes.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `registered`, `identified`, `modes` are future-use.
pub struct ClientInfo {
    pub nick: String,
    pub username: String,
    pub realname: String,
    pub hostname: String,
    pub registered: bool,
    /// Whether the client has identified with NickServ (future use).
    pub identified: bool,
    /// User-level mode flags (e.g. `+i` invisible). Stored as a set of chars.
    pub modes: String,
}

impl ClientInfo {
    /// Build a `nick!user@host` prefix string.
    pub fn prefix(&self) -> String {
        format!("{}!{}@{}", self.nick, self.username, self.hostname)
    }
}

// ---------------------------------------------------------------------------
// ClientHandle
// ---------------------------------------------------------------------------

/// A handle to a connected client.
///
/// Cheap to clone — the expensive part (the mpsc sender) is behind an `Arc`
/// internally in tokio. Cloning a `ClientHandle` does **not** duplicate the
/// connection; it merely gives another way to push lines to the client's
/// write task.
#[derive(Debug, Clone)]
pub struct ClientHandle {
    pub id: ClientId,
    pub info: ClientInfo,
    /// Sender half of the channel to the client's writer task.
    tx: mpsc::Sender<String>,
    /// Server name, cached so we can build numeric replies cheaply.
    server_name: String,
}

impl ClientHandle {
    /// Create a new handle.
    pub fn new(
        id: ClientId,
        info: ClientInfo,
        tx: mpsc::Sender<String>,
        server_name: String,
    ) -> Self {
        Self {
            id,
            info,
            tx,
            server_name,
        }
    }

    /// The client's `nick!user@host` prefix.
    pub fn prefix(&self) -> String {
        self.info.prefix()
    }

    /// Send a pre-built `IrcMessage` to this client.
    pub fn send_message(&self, msg: &IrcMessage) {
        let line = msg.serialize();
        // Fire-and-forget: if the channel is full or closed the client is gone.
        let _ = self.tx.try_send(line);
    }

    /// Build and send a numeric reply: `:server CODE nick <params...>`.
    pub fn send_numeric(&self, code: u16, params: &[&str]) {
        let msg = IrcMessage::numeric(code, &self.info.nick, params).with_prefix(&self.server_name);
        self.send_message(&msg);
    }

    /// Send a raw IRC line (already serialized).
    #[allow(dead_code)] // Future use for direct raw sends.
    pub fn send_raw(&self, line: String) {
        let _ = self.tx.try_send(line);
    }

    /// The underlying sender, exposed for the connection writer task.
    #[allow(dead_code)] // Future use for external write access.
    pub fn sender(&self) -> &mpsc::Sender<String> {
        &self.tx
    }
}
