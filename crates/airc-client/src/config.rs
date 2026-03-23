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
    /// Password used for authentication.
    ///
    /// When set, the client auto-negotiates the best available SASL mechanism
    /// advertised by the server (SCRAM-SHA-256 → PLAIN → NickServ IDENTIFY).
    /// If the server does not advertise SASL, a NickServ IDENTIFY is sent
    /// after RPL_WELCOME as a fallback.
    pub password: Option<String>,
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

    /// Set the password used for authentication.
    ///
    /// The client will auto-negotiate the best SASL mechanism advertised by
    /// the server (SCRAM-SHA-256 → PLAIN), falling back to a NickServ
    /// IDENTIFY if the server does not support SASL.
    #[must_use]
    pub fn with_password(mut self, password: &str) -> Self {
        self.password = Some(password.to_string());
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
