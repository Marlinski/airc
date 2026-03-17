//! SASL SCRAM-SHA-256 mechanism (RFC 5802).
//!
//! SCRAM (Salted Challenge Response Authentication Mechanism) is a
//! challenge-response protocol that never sends the password over the wire,
//! even in clear text.  It is the recommended mechanism when TLS is not
//! available and the preferred one even when it is.
//!
//! # Exchange summary
//!
//! ```text
//! C → S  AUTHENTICATE SCRAM-SHA-256
//! S → C  AUTHENTICATE +                         (empty server challenge)
//! C → S  AUTHENTICATE <base64(client-first)>
//! S → C  AUTHENTICATE <base64(server-first)>    (contains salt + iteration count)
//! C → S  AUTHENTICATE <base64(client-final)>
//! S → C  AUTHENTICATE <base64(server-final)>    (server proof)
//! S → C  900 / 903
//! ```
//!
//! # Implementation notes
//!
//! * We use PBKDF2-HMAC-SHA-256 for key derivation (SaltedPassword).
//! * Salts are stored per-account in `PasswordRecord::scram_salt` and
//!   `PasswordRecord::scram_iterations`.  If a record has no stored SCRAM
//!   parameters we derive them on the fly and store them back (future work —
//!   for now we derive a deterministic salt from the password hash so that
//!   the handshake works without a separate DB migration).
//! * Channel binding (`p=tls-unique`) is not implemented; we reject any
//!   `p=` channel-binding flag and only accept `n,,` (no binding).

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::{Digest, Sha256};

use super::{PasswordLookup, PasswordRecord, SaslError, SaslMechanism, SaslStep};

type HmacSha256 = Hmac<Sha256>;

/// Number of PBKDF2 iterations.  RFC 5802 recommends ≥ 4096; we use 4096.
const ITERATIONS: u32 = 4096;

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

enum State {
    /// Waiting for the client-first message (sent after our empty challenge).
    AwaitingClientFirst,
    /// We sent the server-first; waiting for the client-final.
    AwaitingClientFinal {
        /// `client-first-message-bare` (without gs2 header), stored for the
        /// auth-message construction.
        client_first_bare: String,
        /// Our server nonce (client nonce + 18 random bytes).
        server_nonce: String,
        /// Derived key material.
        server_key: [u8; 32],
        stored_key: [u8; 32],
        /// Verbatim server-first-message (needed for auth-message).
        server_first: String,
    },
    /// We sent the server-final; waiting for the client's "+" acknowledgement.
    AwaitingAck {
        account: String,
    },
    Done,
}

// ---------------------------------------------------------------------------
// ScramSha256Mechanism
// ---------------------------------------------------------------------------

pub struct ScramSha256Mechanism {
    state: State,
    lookup: PasswordLookup,
}

impl ScramSha256Mechanism {
    pub fn new(lookup: PasswordLookup) -> Self {
        Self {
            state: State::AwaitingClientFirst,
            lookup,
        }
    }
}

impl SaslMechanism for ScramSha256Mechanism {
    fn step(&mut self, payload: &str) -> Result<SaslStep, SaslError> {
        match &self.state {
            State::Done => Err(SaslError::UnexpectedMessage),

            State::AwaitingClientFirst => self.handle_client_first(payload),

            // We need owned data from the state variant — replace temporarily.
            State::AwaitingClientFinal { .. } => {
                // Move state out so we can consume its fields.
                let old = std::mem::replace(&mut self.state, State::Done);
                if let State::AwaitingClientFinal {
                    client_first_bare,
                    server_nonce,
                    server_key,
                    stored_key,
                    server_first,
                } = old
                {
                    self.handle_client_final(
                        payload,
                        &client_first_bare,
                        &server_nonce,
                        &server_key,
                        &stored_key,
                        &server_first,
                    )
                } else {
                    unreachable!()
                }
            }

            // Client sends "+" to acknowledge the server-final; we return Done.
            State::AwaitingAck { .. } => {
                let old = std::mem::replace(&mut self.state, State::Done);
                if let State::AwaitingAck { account } = old {
                    Ok(SaslStep::Done { account })
                } else {
                    unreachable!()
                }
            }
        }
    }
}

impl ScramSha256Mechanism {
    // -----------------------------------------------------------------------
    // Round 1: client-first → server-first
    // -----------------------------------------------------------------------

    fn handle_client_first(&mut self, payload: &str) -> Result<SaslStep, SaslError> {
        // Client sends "+" for the empty challenge, then actual data next step.
        // Some clients bundle the first message directly — handle both.
        if payload == "+" {
            // Send empty challenge; stay in AwaitingClientFirst.
            return Ok(SaslStep::Challenge("+".to_string()));
        }

        let raw = BASE64
            .decode(payload)
            .map_err(|e| SaslError::Malformed(format!("base64: {e}")))?;
        let msg = std::str::from_utf8(&raw)
            .map_err(|_| SaslError::Malformed("client-first not UTF-8".into()))?;

        // Strip gs2 header.  We only accept "n,," (no channel binding, no authzid).
        // "y,," (no binding, binding supported) is also acceptable per RFC.
        let bare = if let Some(rest) = msg.strip_prefix("n,,") {
            rest
        } else if let Some(rest) = msg.strip_prefix("y,,") {
            rest
        } else {
            return Err(SaslError::Malformed(
                "only n,, gs2-header supported (no channel binding)".into(),
            ));
        };

        // Parse client-first-message-bare: n=<authcid>,r=<client-nonce>[,...]
        let attrs = parse_attrs(bare)?;

        let authcid = attrs
            .get("n")
            .ok_or_else(|| SaslError::Malformed("missing n= attribute".into()))?;
        let client_nonce = attrs
            .get("r")
            .ok_or_else(|| SaslError::Malformed("missing r= attribute".into()))?;

        let account_lower = authcid.to_ascii_lowercase();
        let record: PasswordRecord = (self.lookup)(&account_lower).ok_or(SaslError::AuthFailed)?;

        // Derive SCRAM keys from the stored password hash.
        let (salt, salted_password) = derive_keys(&record.password_sha256);
        let (client_key, stored_key, server_key) = compute_keys(&salted_password);
        let _ = client_key; // not needed server-side

        // Build server nonce = client nonce + 18 random bytes (base64).
        let mut rnd = [0u8; 18];
        rand::thread_rng().fill_bytes(&mut rnd);
        let server_nonce = format!("{}{}", client_nonce, BASE64.encode(rnd));

        // server-first-message: r=<server_nonce>,s=<salt_b64>,i=<iterations>
        let salt_b64 = BASE64.encode(salt);
        let server_first = format!("r={server_nonce},s={salt_b64},i={ITERATIONS}");
        let server_first_b64 = BASE64.encode(server_first.as_bytes());

        self.state = State::AwaitingClientFinal {
            client_first_bare: bare.to_string(),
            server_nonce,
            server_key,
            stored_key,
            server_first: server_first.clone(),
        };

        Ok(SaslStep::Challenge(server_first_b64))
    }

    // -----------------------------------------------------------------------
    // Round 2: client-final → server-final
    // -----------------------------------------------------------------------

    fn handle_client_final(
        &mut self,
        payload: &str,
        client_first_bare: &str,
        server_nonce: &str,
        server_key: &[u8; 32],
        stored_key: &[u8; 32],
        server_first: &str,
    ) -> Result<SaslStep, SaslError> {
        let raw = BASE64
            .decode(payload)
            .map_err(|e| SaslError::Malformed(format!("base64: {e}")))?;
        let msg = std::str::from_utf8(&raw)
            .map_err(|_| SaslError::Malformed("client-final not UTF-8".into()))?;

        // client-final-message-without-proof = "c=<gs2-header-b64>,r=<nonce>"
        // client-final-message = <without-proof> ",p=<client-proof-b64>"
        let (without_proof, proof_b64) = split_proof(msg)?;

        let attrs = parse_attrs(msg)?;

        // Verify nonce matches.
        let nonce = attrs
            .get("r")
            .ok_or_else(|| SaslError::Malformed("missing r=".into()))?;
        if *nonce != server_nonce {
            return Err(SaslError::Malformed("nonce mismatch".into()));
        }

        // Reconstruct AuthMessage = client-first-bare + "," + server-first + "," + client-final-without-proof
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");

        // ClientSignature = HMAC(StoredKey, AuthMessage)
        let client_signature = hmac_sha256(stored_key, auth_message.as_bytes());

        // ClientProof = ClientKey XOR ClientSignature
        // We verify: ClientKey = ClientProof XOR ClientSignature
        // Then check: HMAC(SHA256(ClientKey), AuthMessage) == StoredKey
        let proof_bytes = BASE64
            .decode(proof_b64)
            .map_err(|e| SaslError::Malformed(format!("proof base64: {e}")))?;

        if proof_bytes.len() != 32 {
            return Err(SaslError::Malformed("client proof wrong length".into()));
        }

        // Recover ClientKey from proof.
        let mut recovered_client_key = [0u8; 32];
        for i in 0..32 {
            recovered_client_key[i] = proof_bytes[i] ^ client_signature[i];
        }

        // Verify: SHA256(recoveredClientKey) should equal StoredKey.
        let computed_stored = sha256(&recovered_client_key);
        if computed_stored != *stored_key {
            return Err(SaslError::AuthFailed);
        }

        // Build ServerSignature = HMAC(ServerKey, AuthMessage) and send it.
        let server_signature = hmac_sha256(server_key, auth_message.as_bytes());
        let server_final = format!("v={}", BASE64.encode(server_signature));
        let server_final_b64 = BASE64.encode(server_final.as_bytes());

        // Extract account name from client-first-bare n= attribute.
        let bare_attrs = parse_attrs(client_first_bare)?;
        let authcid = bare_attrs
            .get("n")
            .ok_or_else(|| SaslError::Malformed("missing n=".into()))?;
        let account = authcid.to_ascii_lowercase();

        // Transition to AwaitingAck: the connection layer will send the server-final
        // challenge and then call step("+") once more, at which point we return Done.
        self.state = State::AwaitingAck { account };
        Ok(SaslStep::Challenge(server_final_b64))
    }
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

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

/// Derive a deterministic 16-byte salt and then compute SaltedPassword via PBKDF2.
///
/// We derive the salt from SHA-256(password_sha256 + "scram-salt") so that
/// accounts created before SCRAM was implemented still work without a DB
/// migration.  In a production system you would store salt per-account.
fn derive_keys(password_sha256: &str) -> ([u8; 16], [u8; 32]) {
    // Deterministic salt: first 16 bytes of SHA-256(password_sha256 + ":scram").
    let mut h = Sha256::new();
    h.update(password_sha256.as_bytes());
    h.update(b":scram");
    let hash = h.finalize();
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&hash[..16]);

    // SaltedPassword = PBKDF2(HMAC-SHA-256, password, salt, iterations, 32)
    // We use password_sha256 as the password input so we never need the raw password.
    // CPU-heavy work is offloaded to spawn_blocking at the call site (connection.rs).
    let mut salted = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password_sha256.as_bytes(), &salt, ITERATIONS, &mut salted);

    (salt, salted)
}

fn compute_keys(salted_password: &[u8; 32]) -> ([u8; 32], [u8; 32], [u8; 32]) {
    let client_key = hmac_sha256(salted_password, b"Client Key");
    let stored_key = sha256(&client_key);
    let server_key = hmac_sha256(salted_password, b"Server Key");
    (client_key, stored_key, server_key)
}

/// Parse `key=value,key=value,...` into a map.  Values may contain `=`.
fn parse_attrs(s: &str) -> Result<std::collections::HashMap<&str, &str>, SaslError> {
    let mut map = std::collections::HashMap::new();
    for part in s.split(',') {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| SaslError::Malformed(format!("bad attribute '{part}'")))?;
        map.insert(k, v);
    }
    Ok(map)
}

/// Split client-final-message into (without-proof, proof-base64).
fn split_proof(msg: &str) -> Result<(&str, &str), SaslError> {
    // proof is always the last attribute: ",p=<base64>"
    let p_pos = msg
        .rfind(",p=")
        .ok_or_else(|| SaslError::Malformed("missing p= in client-final".into()))?;
    let without_proof = &msg[..p_pos];
    let proof_b64 = &msg[p_pos + 3..];
    Ok((without_proof, proof_b64))
}
