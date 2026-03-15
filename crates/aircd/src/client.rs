//! Per-client data and the handle used to communicate with a connected client.

use std::fmt;
use std::sync::Arc;

use airc_shared::IrcMessage;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// NodeId — identifies a remote aircd instance in a cluster
// ---------------------------------------------------------------------------

/// Opaque identifier for a remote aircd node (auto-generated UUID at startup).
///
/// In single-instance mode this type exists but is never instantiated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// ClientId
// ---------------------------------------------------------------------------

/// Unique, opaque identifier for a locally connected client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// ClientKind — unified representation of a user in the network
// ---------------------------------------------------------------------------

/// Whether a user is locally connected or present on a remote node.
///
/// Used in both the global nick registry (`nick_to_kind`) and channel
/// membership maps. For local clients, the `ClientId` resolves to a
/// `ClientHandle` with an mpsc sender. For remote clients, the `NodeId`
/// tells us which node to relay messages to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClientKind {
    /// Connected to this instance — has a `ClientHandle` in the `clients` map.
    Local(ClientId),
    /// Connected to a remote node — reachable via the relay.
    #[allow(dead_code)] // Used when relay is wired up.
    Remote(NodeId),
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

    /// Check whether the user has a given mode flag (e.g. `'o'`, `'S'`).
    pub fn has_mode(&self, flag: char) -> bool {
        self.modes.contains(flag)
    }

    /// Add a mode flag if not already present. Returns a new `ClientInfo`.
    pub fn with_mode(&self, flag: char) -> ClientInfo {
        if self.modes.contains(flag) {
            return self.clone();
        }
        let mut new = self.clone();
        new.modes.push(flag);
        new
    }

    /// Check whether this user is an IRC operator (`+o`).
    pub fn is_oper(&self) -> bool {
        self.has_mode('o')
    }

    /// Check whether this user is a service (`+S`).
    pub fn is_service(&self) -> bool {
        self.has_mode('S')
    }
}

// ---------------------------------------------------------------------------
// ClientHandle
// ---------------------------------------------------------------------------

/// A handle to a connected client.
///
/// Cheap to clone — `info` is behind `Arc` (atomic refcount bump) and
/// the mpsc sender is behind an `Arc` internally in tokio.
/// Cloning a `ClientHandle` does **not** duplicate the connection;
/// it merely gives another way to push lines to the client's write task.
#[derive(Debug, Clone)]
pub struct ClientHandle {
    pub id: ClientId,
    pub info: Arc<ClientInfo>,
    /// Sender half of the channel to the client's writer task.
    tx: mpsc::Sender<Arc<str>>,
    /// Server name, cached so we can build numeric replies cheaply.
    server_name: Arc<str>,
}

impl ClientHandle {
    /// Create a new handle.
    pub fn new(
        id: ClientId,
        info: Arc<ClientInfo>,
        tx: mpsc::Sender<Arc<str>>,
        server_name: Arc<str>,
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

    /// Send a pre-built `IrcMessage` to this client (serializes the message).
    pub fn send_message(&self, msg: &IrcMessage) {
        let line: Arc<str> = msg.serialize().into();
        // Fire-and-forget: if the channel is full or closed the client is gone.
        let _ = self.tx.try_send(line);
    }

    /// Send a pre-serialized IRC line (as `Arc<str>`) to this client.
    ///
    /// Use this when the same message is being sent to many recipients to
    /// avoid re-serializing the `IrcMessage` for each one.
    pub fn send_line(&self, line: &Arc<str>) {
        let _ = self.tx.try_send(Arc::clone(line));
    }

    /// Build and send a numeric reply: `:server CODE nick <params...>`.
    pub fn send_numeric(&self, code: u16, params: &[&str]) {
        let msg =
            IrcMessage::numeric(code, &self.info.nick, params).with_prefix(&*self.server_name);
        self.send_message(&msg);
    }

    /// Send a raw IRC line (already serialized).
    #[allow(dead_code)] // Future use for direct raw sends.
    pub fn send_raw(&self, line: String) {
        let arc: Arc<str> = line.into();
        let _ = self.tx.try_send(arc);
    }

    /// The underlying sender, exposed for the connection writer task.
    #[allow(dead_code)] // Future use for external write access.
    pub fn sender(&self) -> &mpsc::Sender<Arc<str>> {
        &self.tx
    }
}
