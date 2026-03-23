//! SASL PLAIN mechanism (RFC 4616).
//!
//! Wire format — one base64-encoded message from the client:
//! ```text
//! <authzid> NUL <authcid> NUL <passwd>
//! ```
//! `authzid` (authorisation identity) is typically empty; `authcid` is the
//! account name the client is authenticating as.
//!
//! PLAIN is a single-round mechanism: the server sends an empty challenge
//! (`AUTHENTICATE +`) and the client replies with their credentials in one
//! shot.
//!
//! **Security:** PLAIN transmits the password in the clear (base64 is not
//! encryption).  It must only be used over a TLS connection.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

use super::{PasswordLookup, PasswordRecord, SaslError, SaslMechanism, SaslStep};
use crate::services::nickserv::bcrypt_verify_password;

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

enum State {
    /// Waiting for the first (and only) client message.
    AwaitingCredentials,
    /// Exchange is complete — any further calls are a protocol error.
    Done,
}

// ---------------------------------------------------------------------------
// PlainMechanism
// ---------------------------------------------------------------------------

/// SASL PLAIN — one-shot password authentication.
pub struct PlainMechanism {
    state: State,
    lookup: PasswordLookup,
}

impl PlainMechanism {
    pub fn new(lookup: PasswordLookup) -> Self {
        Self {
            state: State::AwaitingCredentials,
            lookup,
        }
    }
}

impl SaslMechanism for PlainMechanism {
    fn step(&mut self, payload: &str) -> Result<SaslStep, SaslError> {
        match self.state {
            State::Done => Err(SaslError::UnexpectedMessage),

            State::AwaitingCredentials => {
                self.state = State::Done;

                // "+" means the client is sending credentials immediately
                // (no separate server challenge needed for PLAIN).
                // Clients may also send the credentials directly on the first
                // AUTHENTICATE line without a "+" round.
                if payload == "+" {
                    // Client is asking us to send the empty challenge — but
                    // for PLAIN the very first message *is* the challenge: "+".
                    // We'll treat a bare "+" as "send empty challenge and wait",
                    // but the IRC wire protocol collapses this into one step.
                    // In practice clients always bundle credentials right away.
                    return Err(SaslError::Malformed(
                        "empty payload; send credentials".into(),
                    ));
                }

                let decoded = BASE64
                    .decode(payload)
                    .map_err(|e| SaslError::Malformed(format!("base64: {e}")))?;

                // Format: authzid NUL authcid NUL passwd
                let parts: Vec<&[u8]> = decoded.splitn(3, |b| *b == 0).collect();
                if parts.len() != 3 {
                    return Err(SaslError::Malformed(
                        "expected authzid\\0authcid\\0passwd".into(),
                    ));
                }

                let authcid = std::str::from_utf8(parts[1])
                    .map_err(|_| SaslError::Malformed("authcid not UTF-8".into()))?;
                let passwd = std::str::from_utf8(parts[2])
                    .map_err(|_| SaslError::Malformed("passwd not UTF-8".into()))?;

                let account_lower = authcid.to_ascii_lowercase();

                let record: PasswordRecord =
                    (self.lookup)(&account_lower).ok_or(SaslError::AuthFailed)?;

                if bcrypt_verify_password(passwd, &record.bcrypt_hash) {
                    Ok(SaslStep::Done {
                        account: account_lower,
                    })
                } else {
                    Err(SaslError::AuthFailed)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::nickserv::{bcrypt_hash_password, derive_scram_credentials};
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};

    fn make_lookup(nick: &str, password: &str) -> PasswordLookup {
        let (scram_stored_key, scram_server_key, scram_salt, scram_iterations) =
            derive_scram_credentials(password);
        let bcrypt_hash = bcrypt_hash_password(password);
        let record = PasswordRecord {
            account: nick.to_string(),
            scram_stored_key,
            scram_server_key,
            scram_salt,
            scram_iterations,
            bcrypt_hash,
        };
        Box::new(move |name: &str| {
            if name == record.account {
                Some(record.clone())
            } else {
                None
            }
        })
    }

    fn encode_plain(authzid: &str, authcid: &str, passwd: &str) -> String {
        let mut raw = Vec::new();
        raw.extend_from_slice(authzid.as_bytes());
        raw.push(0);
        raw.extend_from_slice(authcid.as_bytes());
        raw.push(0);
        raw.extend_from_slice(passwd.as_bytes());
        BASE64.encode(&raw)
    }

    #[test]
    fn plain_correct_password() {
        let mut m = PlainMechanism::new(make_lookup("alice", "s3cr3t"));
        let payload = encode_plain("", "alice", "s3cr3t");
        match m.step(&payload).unwrap() {
            SaslStep::Done { account } => assert_eq!(account, "alice"),
            SaslStep::Challenge(_) => panic!("expected Done"),
        }
    }

    #[test]
    fn plain_wrong_password() {
        let mut m = PlainMechanism::new(make_lookup("alice", "s3cr3t"));
        let payload = encode_plain("", "alice", "wrong");
        assert!(matches!(m.step(&payload), Err(SaslError::AuthFailed)));
    }

    #[test]
    fn plain_unknown_account() {
        let mut m = PlainMechanism::new(make_lookup("alice", "s3cr3t"));
        let payload = encode_plain("", "bob", "s3cr3t");
        assert!(matches!(m.step(&payload), Err(SaslError::AuthFailed)));
    }

    #[test]
    fn plain_bad_base64() {
        let mut m = PlainMechanism::new(make_lookup("alice", "s3cr3t"));
        assert!(matches!(
            m.step("not-valid-base64!!!"),
            Err(SaslError::Malformed(_))
        ));
    }

    #[test]
    fn plain_case_insensitive_authcid() {
        let mut m = PlainMechanism::new(make_lookup("alice", "s3cr3t"));
        let payload = encode_plain("", "ALICE", "s3cr3t");
        match m.step(&payload).unwrap() {
            SaslStep::Done { account } => assert_eq!(account, "alice"),
            SaslStep::Challenge(_) => panic!("expected Done"),
        }
    }
}
