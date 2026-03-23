//! SASL authentication layer for AIRC.
//!
//! # Design
//!
//! Each SASL mechanism is an independent struct that implements [`SaslMechanism`].
//! A mechanism is a state machine: calling [`SaslMechanism::step`] advances it
//! one round, returning either a challenge to send back to the client, a
//! successful authentication result, or an error.
//!
//! The [`SaslSession`] type wraps a boxed [`SaslMechanism`] and owns the
//! in-progress exchange during the connection registration phase.
//!
//! # Supported mechanisms
//!
//! | Name            | Module       | Notes                             |
//! |-----------------|--------------|-----------------------------------|
//! | `PLAIN`         | [`plain`]    | Requires TLS; trivial one-shot    |
//! | `SCRAM-SHA-256` | [`scram`]    | Challenge-response; safe over TLS |
//!
//! # Wire protocol
//!
//! ```text
//! C: AUTHENTICATE PLAIN
//! S: AUTHENTICATE +          ← server ready (empty challenge)
//! C: AUTHENTICATE <base64>   ← client sends credentials
//! S: 900 ...                 ← RPL_LOGGEDIN
//! S: 903 :SASL authentication successful
//! ```
//!
//! For SCRAM-SHA-256 the exchange is two rounds (client-first → server-first
//! → client-final → server-final).

pub mod error;
pub mod plain;
pub mod scram;

pub use error::SaslError;

// ---------------------------------------------------------------------------
// SaslStep — what a mechanism returns after each round
// ---------------------------------------------------------------------------

/// The result of one step in a SASL exchange.
pub enum SaslStep {
    /// Send this challenge to the client and wait for their next message.
    /// The inner string is already base64-encoded (or `"+"` for empty).
    Challenge(String),

    /// Authentication is complete.  `account` is the canonical (lowercase)
    /// account name that was authenticated.
    Done { account: String },
}

// ---------------------------------------------------------------------------
// SaslMechanism trait
// ---------------------------------------------------------------------------

/// A single SASL mechanism.
///
/// Implementations are one-shot state machines — once `step` returns
/// `SaslStep::Done` or `Err`, the mechanism must not be called again.
pub trait SaslMechanism: Send {
    /// Advance the exchange by one round.
    ///
    /// `payload` is the raw (not base64-decoded) string received from the
    /// `AUTHENTICATE` command.  An empty payload is represented by `"+"`.
    fn step(&mut self, payload: &str) -> Result<SaslStep, SaslError>;
}

// ---------------------------------------------------------------------------
// SaslSession — owns a live exchange
// ---------------------------------------------------------------------------

/// Wraps a boxed [`SaslMechanism`] for use in the connection registration loop.
pub struct SaslSession {
    #[allow(dead_code)]
    pub mechanism_name: String,
    mechanism: Box<dyn SaslMechanism>,
}

impl SaslSession {
    /// Advance the exchange by one round.
    pub fn step(&mut self, payload: &str) -> Result<SaslStep, SaslError> {
        self.mechanism.step(payload)
    }
}

// ---------------------------------------------------------------------------
// Registry — build a SaslSession by mechanism name
// ---------------------------------------------------------------------------

/// Instantiate a SASL session for the named mechanism, backed by `lookup`.
///
/// `lookup` is an async closure that maps an account name (lowercase) to the
/// stored [`PasswordRecord`] for that account, if any.  This decouples the
/// SASL layer from NickServ internals.
///
/// Returns `None` if the mechanism name is not supported.
pub fn new_session(mechanism_name: &str, lookup: PasswordLookup) -> Option<SaslSession> {
    let mechanism: Box<dyn SaslMechanism> = match mechanism_name {
        "PLAIN" => Box::new(plain::PlainMechanism::new(lookup)),
        "SCRAM-SHA-256" => Box::new(scram::ScramSha256Mechanism::new(lookup)),
        _ => return None,
    };
    Some(SaslSession {
        mechanism_name: mechanism_name.to_string(),
        mechanism,
    })
}

/// Comma-separated list of supported mechanism names (for CAP LS / 908).
pub const SUPPORTED_MECHANISMS: &str = "PLAIN,SCRAM-SHA-256";

// ---------------------------------------------------------------------------
// PasswordRecord — credential data passed to mechanisms via lookup
// ---------------------------------------------------------------------------

/// The stored credential data for one account.
///
/// At registration time the server derives all key material from the raw
/// password and stores only what is needed for each mechanism:
///
/// - **SCRAM-SHA-256**: `(stored_key, server_key, scram_salt, scram_iterations)`.
///   These are used directly at login — no PBKDF2 at login time.
/// - **PLAIN**: `bcrypt_hash` — verified with `bcrypt::verify`.
///
/// The raw password is never stored.
#[derive(Clone)]
pub struct PasswordRecord {
    /// Lowercase account name.
    #[allow(dead_code)]
    pub account: String,
    /// StoredKey = SHA256(HMAC(SaltedPassword, "Client Key")), hex-encoded.
    pub scram_stored_key: String,
    /// ServerKey = HMAC(SaltedPassword, "Server Key"), hex-encoded.
    pub scram_server_key: String,
    /// Random 16-byte salt used for PBKDF2, hex-encoded.
    pub scram_salt: String,
    /// PBKDF2 iteration count used when deriving SaltedPassword.
    pub scram_iterations: u32,
    /// bcrypt hash of the password (for PLAIN auth).
    pub bcrypt_hash: String,
}

// ---------------------------------------------------------------------------
// PasswordLookup — sync callback used by mechanisms
// ---------------------------------------------------------------------------

/// A sync callback that resolves an account name to its [`PasswordRecord`].
///
/// Both PLAIN and SCRAM receive this at construction time.  Using a callback
/// (rather than an `Arc<NickServState>`) keeps the mechanisms self-contained
/// and easily testable.
pub type PasswordLookup = Box<dyn Fn(&str) -> Option<PasswordRecord> + Send>;
