//! NickServ cryptographic utilities and time helpers.
//!
//! All JSON persistence functions have been removed. Persistent state is
//! stored exclusively via the CRDT-backed `PersistentState` (SQLite).

use std::time::{SystemTime, UNIX_EPOCH};

use hex;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

/// Hash a password with SHA-256, returning a lowercase hex string.
///
/// Used by SASL PLAIN and SCRAM-SHA-256 for credential verification, and by
/// NickServ REGISTER/IDENTIFY for storing and checking passwords.
pub fn hash_password(password: &str) -> String {
    let mut h = Sha256::new();
    h.update(password.as_bytes());
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// Hex encoding / decoding
// ---------------------------------------------------------------------------

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

pub(crate) fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

pub(crate) fn parse_pubkey(hex: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes = hex_decode(hex)?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()
}
