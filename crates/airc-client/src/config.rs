//! Client configuration.

/// Default IRC server address.
pub const DEFAULT_SERVER: &str = "irc.openlore.xyz:6697";

/// Default nickname.
pub const DEFAULT_NICK: &str = "agent";

/// TLS mode for IRC connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// Always use TLS (fail if TLS handshake fails).
    Required,
    /// Try TLS first, fall back to plain TCP on failure.
    Preferred,
    /// Never use TLS (plain TCP only).
    Disabled,
}

/// SASL authentication mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslMechanism {
    /// PLAIN — sends credentials base64-encoded in clear text.
    /// Safe over TLS; not safe over plain TCP.
    Plain,
    /// SCRAM-SHA-256 — challenge-response; credentials never sent in clear text.
    ScramSha256,
}

impl SaslMechanism {
    /// The wire name used in `AUTHENTICATE <name>`.
    pub fn wire_name(self) -> &'static str {
        match self {
            SaslMechanism::Plain => "PLAIN",
            SaslMechanism::ScramSha256 => "SCRAM-SHA-256",
        }
    }
}

/// SASL credentials for connection-time authentication.
///
/// When present in [`ClientConfig`], the client will negotiate the `sasl`
/// capability and complete a SASL exchange before sending `CAP END`.
#[derive(Debug, Clone)]
pub struct SaslConfig {
    /// Mechanism to use.
    pub mechanism: SaslMechanism,
    /// Account name (authcid). Usually the same as the nick.
    pub account: String,
    /// Password.
    pub password: String,
}

impl SaslConfig {
    /// Create a new SASL PLAIN config.
    pub fn plain(account: &str, password: &str) -> Self {
        Self {
            mechanism: SaslMechanism::Plain,
            account: account.to_string(),
            password: password.to_string(),
        }
    }

    /// Create a new SASL SCRAM-SHA-256 config.
    pub fn scram_sha256(account: &str, password: &str) -> Self {
        Self {
            mechanism: SaslMechanism::ScramSha256,
            account: account.to_string(),
            password: password.to_string(),
        }
    }
}

/// Configuration for an IRC client connection.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Server address in `host:port` format.
    pub server_addr: String,
    /// Desired nickname.
    pub nick: String,
    /// Username (ident). Defaults to the nick if not set.
    pub username: String,
    /// Real name / description. Defaults to the nick if not set.
    pub realname: String,
    /// Connection password (optional, sent as PASS before NICK/USER).
    pub password: Option<String>,
    /// SASL credentials (optional). When set, the client performs a SASL
    /// handshake during connection registration before `CAP END`.
    pub sasl: Option<SaslConfig>,
    /// Channels to auto-join after registration.
    pub auto_join: Vec<String>,
    /// Maximum number of messages to buffer per channel.
    pub buffer_size: usize,
    /// TLS mode. Defaults to [`TlsMode::Preferred`].
    pub tls: TlsMode,
}

impl ClientConfig {
    /// Create a new config with sensible defaults.
    pub fn new(server_addr: &str, nick: &str) -> Self {
        Self {
            server_addr: server_addr.to_string(),
            nick: nick.to_string(),
            username: nick.to_string(),
            realname: nick.to_string(),
            password: None,
            sasl: None,
            auto_join: Vec::new(),
            buffer_size: 1000,
            tls: TlsMode::Preferred,
        }
    }

    /// Set the username.
    #[must_use]
    pub fn with_username(mut self, username: &str) -> Self {
        self.username = username.to_string();
        self
    }

    /// Set the realname.
    #[must_use]
    pub fn with_realname(mut self, realname: &str) -> Self {
        self.realname = realname.to_string();
        self
    }

    /// Set the connection password.
    #[must_use]
    pub fn with_password(mut self, password: &str) -> Self {
        self.password = Some(password.to_string());
        self
    }

    /// Configure SASL authentication.
    ///
    /// When set, the client will negotiate the `sasl` capability and perform
    /// a SASL handshake before completing registration.
    #[must_use]
    pub fn with_sasl(mut self, sasl: SaslConfig) -> Self {
        self.sasl = Some(sasl);
        self
    }

    /// Add channels to auto-join.
    #[must_use]
    pub fn with_auto_join(mut self, channels: Vec<String>) -> Self {
        self.auto_join = channels;
        self
    }

    /// Set the per-channel buffer size.
    #[must_use]
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// Set the TLS mode.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsMode) -> Self {
        self.tls = tls;
        self
    }
}
