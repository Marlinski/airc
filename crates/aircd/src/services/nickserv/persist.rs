//! NickServ cryptographic utilities and time helpers.
//!
//! All JSON persistence functions have been removed. Persistent state is
//! stored exclusively via the CRDT-backed `PersistentState` (SQLite).
//!
//! # Credential design
//!
//! At **registration** time, `derive_scram_credentials` is called with the raw
//! password.  It generates a random 16-byte salt, runs PBKDF2-HMAC-SHA-256
//! with a secure iteration count (≥ 600 000), and returns the five values that
//! must be stored:
//!
//! ```text
//! SaltedPassword = PBKDF2(HMAC-SHA-256, password, salt, iterations)
//! ClientKey      = HMAC(SaltedPassword, "Client Key")
//! StoredKey      = SHA-256(ClientKey)
//! ServerKey      = HMAC(SaltedPassword, "Server Key")
//! ```
//!
//! `bcrypt_hash_password` produces a bcrypt hash for use with SASL PLAIN.
//!
//! Neither function is called at login time — at login the server reads the
//! pre-computed values from the database and uses them directly.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

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
// SCRAM credential derivation
// ---------------------------------------------------------------------------

/// PBKDF2 iteration count.  OWASP 2023 recommends ≥ 600 000 for
/// PBKDF2-HMAC-SHA-256.
pub const SCRAM_ITERATIONS: u32 = 600_000;

/// Derive SCRAM-SHA-256 credential material from a raw password.
///
/// Returns `(stored_key_hex, server_key_hex, salt_hex, iterations)`.
///
/// This is the only function that needs the raw password; call it once at
/// registration time and store the four return values.  Discard the raw
/// password immediately after.
pub fn derive_scram_credentials(password: &str) -> (String, String, String, u32) {
    // Generate a random 16-byte salt.
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);

    let (stored_key, server_key) = scram_keys_from_password(password, &salt, SCRAM_ITERATIONS);

    let stored_key_hex = hex::encode(stored_key);
    let server_key_hex = hex::encode(server_key);
    let salt_hex = hex::encode(salt);

    (stored_key_hex, server_key_hex, salt_hex, SCRAM_ITERATIONS)
}

/// Compute `(StoredKey, ServerKey)` from a raw password + salt + iterations.
///
/// Exposed for internal testing; production code calls `derive_scram_credentials`.
pub(crate) fn scram_keys_from_password(
    password: &str,
    salt: &[u8],
    iterations: u32,
) -> ([u8; 32], [u8; 32]) {
    // SaltedPassword = PBKDF2(HMAC-SHA-256, password, salt, iterations, 32)
    let mut salted = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut salted);

    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key = sha256(&client_key);
    let server_key = hmac_sha256(&salted, b"Server Key");

    (stored_key, server_key)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

// ---------------------------------------------------------------------------
// bcrypt hashing (for PLAIN auth)
// ---------------------------------------------------------------------------

/// bcrypt cost factor.  bcrypt cost 12 ≈ 300 ms on modern hardware, which
/// is acceptable for interactive logins while remaining expensive for
/// offline attacks.
const BCRYPT_COST: u32 = 12;

/// Hash a password with bcrypt.  Call once at registration; store the result.
pub fn bcrypt_hash_password(password: &str) -> String {
    bcrypt::hash(password, BCRYPT_COST).expect("bcrypt hash failed")
}

/// Verify a plain-text password against a stored bcrypt hash.
pub fn bcrypt_verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Hex encoding / decoding
// ---------------------------------------------------------------------------

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

pub(crate) fn hex_decode(hex_str: &str) -> Option<Vec<u8>> {
    if !hex_str.len().is_multiple_of(2) {
        return None;
    }
    hex::decode(hex_str).ok()
}

pub(crate) fn parse_pubkey(hex: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes = hex_decode(hex)?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&arr).ok()
}
