//! NickServ data types — identity and challenge records.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A registered nick identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// The canonical (original casing) nickname.
    pub nick: String,
    /// StoredKey for SCRAM-SHA-256, hex-encoded.
    /// `None` if the user registered with a keypair only.
    pub scram_stored_key: Option<String>,
    /// ServerKey for SCRAM-SHA-256, hex-encoded.
    pub scram_server_key: Option<String>,
    /// Random 16-byte PBKDF2 salt, hex-encoded.
    pub scram_salt: Option<String>,
    /// PBKDF2 iteration count used during registration.
    pub scram_iterations: Option<u32>,
    /// bcrypt hash of the password (for PLAIN auth).
    pub bcrypt_hash: Option<String>,
    /// Ed25519 public key in hex, if registered via keypair.
    pub pubkey_hex: Option<String>,
    /// Unix timestamp of registration.
    pub registered_at: u64,
    /// Reputation score.
    pub reputation: i64,
    /// Declared capabilities (free-form strings).
    pub capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// Pending keypair challenge
// ---------------------------------------------------------------------------

#[allow(dead_code)] // nick_lower kept for future audit/logging.
pub(crate) struct PendingChallenge {
    pub nonce: [u8; 32],
    pub nick_lower: String,
    /// Unix timestamp when the challenge was created (for expiry).
    pub created_at: u64,
}

// ---------------------------------------------------------------------------
// Silence entry (returned by NickServState::get_silence_list)
// ---------------------------------------------------------------------------

/// A single silenced nick with optional reason.
#[derive(Debug, Clone)]
pub struct SilenceEntry {
    /// Lowercase nick of the silenced user.
    pub nick: String,
    /// Optional reason for silencing.
    pub reason: Option<String>,
}
