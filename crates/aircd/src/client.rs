//! Per-client data and the handle used to communicate with a connected client.

use std::fmt;
use std::sync::Arc;

use airc_shared::IrcMessage;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

// ---------------------------------------------------------------------------
// NodeId — identifies a remote aircd instance in a cluster
// ---------------------------------------------------------------------------

/// Opaque identifier for a remote aircd node (auto-generated UUID at startup).
///
/// In single-instance mode this type exists but is never instantiated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// ClientId
// ---------------------------------------------------------------------------

/// Unique, opaque identifier for a user in the network (local or remote).
///
/// Local clients receive a monotonically-increasing integer ID at connection
/// time.  Remote clients receive a `ClientId` that was assigned by their home
/// node and is forwarded in the `ClientIntro` S2S message.  Once assigned a
/// `ClientId` never changes for the lifetime of the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(pub u64);

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// ClientKind — local vs remote transport
// ---------------------------------------------------------------------------

/// Whether a user is locally connected or present on a remote node.
///
/// A `Local` client has an mpsc sender we can write to directly.  A `Remote`
/// client is reachable only via the relay bus — we record which node owns it
/// so we can clean up on `NodeDown`.
#[derive(Debug, Clone)]
pub enum ClientKind {
    /// Connected to this instance.
    Local {
        /// Sender half of the channel to the client's writer task.
        tx: mpsc::Sender<Arc<str>>,
        /// Cancellation token shared with the write loop.
        cancel: CancellationToken,
        /// Server name, cached so we can build numeric replies cheaply.
        server_name: Arc<str>,
    },
    /// Connected to a remote node — reachable via the relay.
    Remote {
        /// Which node owns this client.
        node_id: NodeId,
    },
}

// ---------------------------------------------------------------------------
// User mode bit flags
// ---------------------------------------------------------------------------

/// User mode bit flags.  Each IRC user mode letter maps to one bit in the
/// `ClientInfo::modes` `u32` field.
///
/// Only modes that are actually used by aircd are defined here.  Unknown mode
/// letters received from remote nodes are silently ignored (forward compat).
pub mod user_mode {
    pub const INVISIBLE: u32 = 1 << 0; // +i
    pub const OPER: u32 = 1 << 1; // +o
    pub const SERVICE: u32 = 1 << 2; // +S

    /// Convert a mode flag character to its bitmask, or `None` if unknown.
    pub fn char_to_bit(flag: char) -> Option<u32> {
        match flag {
            'i' => Some(INVISIBLE),
            'o' => Some(OPER),
            'S' => Some(SERVICE),
            _ => None,
        }
    }

    /// Produce a sorted mode string (without the leading `+`) for the bits set
    /// in `flags`.  The canonical IRC order is alphabetical with uppercase last.
    pub fn bits_to_string(flags: u32) -> String {
        let mut s = String::new();
        if flags & INVISIBLE != 0 {
            s.push('i');
        }
        if flags & OPER != 0 {
            s.push('o');
        }
        if flags & SERVICE != 0 {
            s.push('S');
        }
        s
    }
}

// ---------------------------------------------------------------------------
// ClientInfo
// ---------------------------------------------------------------------------

/// Stored data about a client — identity, registration status, modes.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `registered` is future-use.
pub struct ClientInfo {
    pub nick: String,
    pub username: String,
    pub realname: String,
    pub hostname: String,
    pub registered: bool,
    /// Whether the client has identified via SASL or NickServ.
    pub identified: bool,
    /// The NickServ account name if identified (lowercase), or `None`.
    pub account: Option<String>,
    /// User-level mode flags packed as a bitfield.  Use [`user_mode`] constants.
    pub modes: u32,
    /// Away message. `None` = not away, `Some(msg)` = away.
    pub away: Option<String>,
    /// Bitmask of IRCv3 capabilities the client has negotiated (ACK'd).
    /// Use the `cap` module constants to test/set bits.
    pub caps: u32,
}

impl ClientInfo {
    /// Build a `nick!user@host` prefix string.
    pub fn prefix(&self) -> String {
        format!("{}!{}@{}", self.nick, self.username, self.hostname)
    }

    /// Check whether the client has a specific IRCv3 capability active.
    pub fn has_cap(&self, bit: u32) -> bool {
        self.caps & bit != 0
    }

    /// Check whether the user has a given mode flag (e.g. `'o'`, `'S'`).
    #[allow(dead_code)]
    pub fn has_mode(&self, flag: char) -> bool {
        user_mode::char_to_bit(flag).is_some_and(|bit| self.modes & bit != 0)
    }

    /// Return a new `ClientInfo` with the given mode flag set.
    pub fn with_mode(&self, flag: char) -> ClientInfo {
        let mut new = self.clone();
        if let Some(bit) = user_mode::char_to_bit(flag) {
            new.modes |= bit;
        }
        new
    }

    /// Return a new `ClientInfo` with the given mode flag cleared.
    pub fn without_mode(&self, flag: char) -> ClientInfo {
        let mut new = self.clone();
        if let Some(bit) = user_mode::char_to_bit(flag) {
            new.modes &= !bit;
        }
        new
    }

    /// Check whether this user has the invisible mode (`+i`).
    pub fn is_invisible(&self) -> bool {
        self.modes & user_mode::INVISIBLE != 0
    }

    /// Check whether this user is an IRC operator (`+o`).
    pub fn is_oper(&self) -> bool {
        self.modes & user_mode::OPER != 0
    }

    /// Check whether this user is a service (`+S`).
    pub fn is_service(&self) -> bool {
        self.modes & user_mode::SERVICE != 0
    }
}

// ---------------------------------------------------------------------------
// Client — unified handle for local and remote users
// ---------------------------------------------------------------------------

/// A handle to a user in the network (local or remote).
///
/// Cheap to clone — `info` is behind `Arc` (atomic refcount bump) and the
/// mpsc sender (for local clients) is already behind an `Arc` internally.
/// Cloning a `Client` does **not** duplicate the connection; it merely gives
/// another reference to the same user's state.
#[derive(Debug, Clone)]
pub struct Client {
    pub id: ClientId,
    pub info: Arc<ClientInfo>,
    pub kind: ClientKind,
}

/// Alias kept for call-site readability inside local-client code paths.
pub type ClientHandle = Client;

impl Client {
    /// Create a new local client handle.
    pub fn new_local(
        id: ClientId,
        info: Arc<ClientInfo>,
        tx: mpsc::Sender<Arc<str>>,
        cancel: CancellationToken,
        server_name: Arc<str>,
    ) -> Self {
        Self {
            id,
            info,
            kind: ClientKind::Local {
                tx,
                cancel,
                server_name,
            },
        }
    }

    /// Create a new remote client handle.
    pub fn new_remote(id: ClientId, info: Arc<ClientInfo>, node_id: NodeId) -> Self {
        Self {
            id,
            info,
            kind: ClientKind::Remote { node_id },
        }
    }

    /// Returns `true` if this client is connected locally.
    pub fn is_local(&self) -> bool {
        matches!(self.kind, ClientKind::Local { .. })
    }

    /// Returns `true` if this client is on a remote node.
    #[allow(dead_code)]
    pub fn is_remote(&self) -> bool {
        matches!(self.kind, ClientKind::Remote { .. })
    }

    /// The node ID for a remote client, if applicable.
    #[allow(dead_code)]
    pub fn node_id(&self) -> Option<&NodeId> {
        match &self.kind {
            ClientKind::Remote { node_id } => Some(node_id),
            ClientKind::Local { .. } => None,
        }
    }

    /// The client's `nick!user@host` prefix.
    pub fn prefix(&self) -> String {
        self.info.prefix()
    }

    /// Send a pre-built `IrcMessage` to this client (serializes the message).
    ///
    /// No-op for remote clients — messages to remote users are routed via the
    /// relay, not via this method.
    pub fn send_message(&self, msg: &IrcMessage) {
        let line: Arc<str> = msg.serialize().into();
        self.send_line(&line);
    }

    /// Send a message with a `@time=` tag prepended if the client has
    /// `server-time` or `message-tags` capability enabled.  Otherwise
    /// behaves identically to [`send_message`].
    ///
    /// Use this for all user-visible events (PRIVMSG, NOTICE, JOIN, PART,
    /// QUIT, KICK, TOPIC, MODE, NICK, etc.) so that tag-aware clients
    /// receive accurate timestamps.
    pub fn send_message_tagged(&self, msg: &IrcMessage) {
        if self.info.has_cap(cap::MESSAGE_TAGS) || self.info.has_cap(cap::SERVER_TIME) {
            let now = chrono::Utc::now();
            let time_str = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            let mut tagged = msg.clone();
            // Prepend the time tag (it should come first per convention).
            tagged.tags.insert(0, ("time".to_string(), Some(time_str)));
            let line: Arc<str> = tagged.serialize().into();
            self.send_line(&line);
        } else {
            self.send_message(msg);
        }
    }

    /// Send a pre-serialized IRC line (as `Arc<str>`) to this client.
    ///
    /// Use this when the same message is being sent to many recipients to
    /// avoid re-serializing the `IrcMessage` for each one.
    ///
    /// No-op for remote clients.
    ///
    /// If the client's outbound buffer is full the connection is cancelled
    /// immediately — a slow or unresponsive client must not stall other
    /// senders or consume unbounded memory.
    pub fn send_line(&self, line: &Arc<str>) {
        let (tx, cancel) = match &self.kind {
            ClientKind::Local { tx, cancel, .. } => (tx, cancel),
            ClientKind::Remote { .. } => return, // remote — no local sender
        };
        match tx.try_send(Arc::clone(line)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    client_id = %self.id,
                    nick = %self.info.nick,
                    "outbound buffer full — disconnecting slow client"
                );
                cancel.cancel();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Channel already closed — write loop already exiting.
            }
        }
    }

    /// Build and send a numeric reply: `:server CODE nick <params...>`.
    ///
    /// No-op for remote clients.
    pub fn send_numeric(&self, code: u16, params: &[&str]) {
        let server_name = match &self.kind {
            ClientKind::Local { server_name, .. } => server_name.clone(),
            ClientKind::Remote { .. } => return,
        };
        let msg = IrcMessage::numeric(code, &self.info.nick, params).with_prefix(&*server_name);
        self.send_message(&msg);
    }

    /// Send a `NOTICE` from `from` to `target` with the given `text`.
    ///
    /// No-op for remote clients.
    pub fn send_notice(&self, from: &str, target: &str, text: &str) {
        let msg = IrcMessage::notice(target, text).with_prefix(from);
        self.send_message(&msg);
    }

    /// Send a raw IRC line (already serialized).
    #[allow(dead_code)]
    pub fn send_raw(&self, line: String) {
        let arc: Arc<str> = line.into();
        self.send_line(&arc);
    }

    /// The underlying sender, exposed for the connection writer task.
    #[allow(dead_code)]
    pub fn sender(&self) -> Option<&mpsc::Sender<Arc<str>>> {
        match &self.kind {
            ClientKind::Local { tx, .. } => Some(tx),
            ClientKind::Remote { .. } => None,
        }
    }

    /// The cancellation token for this client's write loop.
    #[allow(dead_code)]
    pub fn cancellation_token(&self) -> Option<&CancellationToken> {
        match &self.kind {
            ClientKind::Local { cancel, .. } => Some(cancel),
            ClientKind::Remote { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// IRCv3 capability bit flags
// ---------------------------------------------------------------------------

/// IRCv3 capability bit flags stored in [`ClientInfo::caps`].
pub mod cap {
    /// `message-tags` — client understands `@tag=value` prefixes on messages.
    pub const MESSAGE_TAGS: u32 = 1 << 0;
    /// `echo-message` — client wants its own PRIVMSG/NOTICE echoed back.
    pub const ECHO_MESSAGE: u32 = 1 << 1;
    /// `away-notify` — client wants AWAY broadcasts from channel members.
    pub const AWAY_NOTIFY: u32 = 1 << 2;
    /// `multi-prefix` — client wants all membership prefixes in NAMES/WHO.
    pub const MULTI_PREFIX: u32 = 1 << 3;
    /// `extended-join` — client wants account + realname in JOIN messages.
    pub const EXTENDED_JOIN: u32 = 1 << 4;
    /// `account-notify` — client wants ACCOUNT broadcasts from channel members.
    pub const ACCOUNT_NOTIFY: u32 = 1 << 5;
    /// `server-time` — client wants `@time=` tags on messages.
    pub const SERVER_TIME: u32 = 1 << 6;

    /// Map a capability name (lowercase) to its bit flag.  Returns `None` for
    /// unknown/unsupported capabilities.
    pub fn name_to_bit(name: &str) -> Option<u32> {
        match name {
            "message-tags" => Some(MESSAGE_TAGS),
            "echo-message" => Some(ECHO_MESSAGE),
            "away-notify" => Some(AWAY_NOTIFY),
            "multi-prefix" => Some(MULTI_PREFIX),
            "extended-join" => Some(EXTENDED_JOIN),
            "account-notify" => Some(ACCOUNT_NOTIFY),
            "server-time" => Some(SERVER_TIME),
            _ => None,
        }
    }

    /// Return the list of supported capability names for CAP LS.
    pub fn supported_caps() -> &'static [&'static str] {
        &[
            "message-tags",
            "server-time",
            "echo-message",
            "away-notify",
            "multi-prefix",
            "extended-join",
            "account-notify",
        ]
    }
}
