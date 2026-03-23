//! SASL authentication handlers.
//!
//! Drives the `AUTHENTICATE` challenge/response exchange and handles the
//! SASL result numerics (900 RPL_LOGGEDIN, 903 RPL_SASLSUCCESS,
//! 904 ERR_SASLFAIL, 906 ERR_SASLABORTED).
//!
//! # SCRAM-SHA-256 client-side exchange (RFC 5802)
//!
//! ```text
//! C→S  AUTHENTICATE SCRAM-SHA-256
//! S→C  AUTHENTICATE +                           (empty challenge)
//! C→S  AUTHENTICATE base64(n,,n=<nick>,r=<cnonce>)
//! S→C  AUTHENTICATE base64(r=<snonce>,s=<salt_b64>,i=<iters>)
//! C→S  AUTHENTICATE base64(c=biws,r=<snonce>,p=<proof_b64>)
//! S→C  AUTHENTICATE base64(v=<server_sig_b64>)  (server-final)
//! S→C  900 / 903
//! ```

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use airc_shared::IrcMessage;

use crate::event::IrcEvent;

use super::ConnContext;
use super::cap::{SaslMechanism, SaslStep};

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// AUTHENTICATE challenge handler
// ---------------------------------------------------------------------------

/// Handle an `AUTHENTICATE` message from the server (the challenge step).
pub async fn handle_authenticate(msg: &IrcMessage, ctx: &ConnContext) {
    let payload = msg.params.first().map(|s| s.as_str()).unwrap_or("+");

    let mut guard = ctx.sasl_state.lock().await;
    let Some(ref mut hs) = *guard else {
        // AUTHENTICATE arrived but we didn't initiate SASL — ignore.
        return;
    };

    match hs.step {
        SaslStep::AwaitingChallenge => {
            // Server responded to our AUTHENTICATE <MECH> with a challenge
            // (or "+" for PLAIN / SCRAM initial empty challenge).
            match hs.mechanism {
                SaslMechanism::Plain => {
                    // PLAIN: server sends "+" (empty challenge); we respond with
                    // base64(\0authcid\0password).
                    if payload != "+" {
                        warn!(payload = %payload, "unexpected PLAIN challenge payload, expected '+'");
                        hs.step = SaslStep::Done;
                        drop(guard);
                        let _ = ctx.line_tx.send("AUTHENTICATE *".to_string()).await;
                        let _ = ctx
                            .event_tx
                            .send(IrcEvent::SaslFailed {
                                code: 0,
                                reason: "unexpected PLAIN challenge".to_string(),
                            })
                            .await;
                        let _ = ctx.line_tx.send("CAP END".to_string()).await;
                        return;
                    }

                    let mut raw: Vec<u8> = Vec::new();
                    raw.push(0); // authzid (empty — use authcid)
                    raw.extend_from_slice(hs.nick.as_bytes());
                    raw.push(0);
                    raw.extend_from_slice(hs.password.as_bytes());
                    let encoded = BASE64.encode(&raw);

                    hs.step = SaslStep::AwaitingSuccess;
                    drop(guard);

                    debug!("sending SASL PLAIN credentials");
                    let _ = ctx.line_tx.send(format!("AUTHENTICATE {encoded}")).await;
                }

                SaslMechanism::ScramSha256 => {
                    // SCRAM step 1: server sends "+" (empty challenge).
                    // We respond with client-first-message.
                    if payload != "+" {
                        warn!(payload = %payload, "unexpected SCRAM initial challenge, expected '+'");
                        hs.step = SaslStep::Done;
                        drop(guard);
                        abort_sasl(ctx, "unexpected SCRAM initial challenge").await;
                        return;
                    }

                    // Generate a random client nonce (18 bytes → 24 base64 chars).
                    let client_nonce = generate_nonce();

                    // client-first-message-bare = n=<nick>,r=<cnonce>
                    let client_first_bare = format!("n={},r={}", hs.nick, client_nonce);
                    // full client-first = gs2-header + client-first-bare
                    let client_first = format!("n,,{client_first_bare}");
                    let encoded = BASE64.encode(client_first.as_bytes());

                    hs.client_nonce = client_nonce;
                    hs.client_first_bare = client_first_bare;
                    hs.step = SaslStep::ScramAwaitingServerFirst;
                    drop(guard);

                    debug!("sending SCRAM client-first");
                    let _ = ctx.line_tx.send(format!("AUTHENTICATE {encoded}")).await;
                }
            }
        }

        SaslStep::ScramAwaitingServerFirst => {
            // SCRAM step 2: server-first-message = r=<snonce>,s=<salt_b64>,i=<iters>
            if payload == "+" || payload.is_empty() {
                warn!("received empty SCRAM server-first, aborting");
                hs.step = SaslStep::Done;
                drop(guard);
                abort_sasl(ctx, "empty SCRAM server-first").await;
                return;
            }

            let raw = match BASE64.decode(payload) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "failed to decode SCRAM server-first");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "invalid base64 in SCRAM server-first").await;
                    return;
                }
            };
            let server_first = match std::str::from_utf8(&raw) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    warn!("SCRAM server-first is not valid UTF-8");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "SCRAM server-first not UTF-8").await;
                    return;
                }
            };

            // Parse: r=<snonce>,s=<salt_b64>,i=<iters>
            let attrs = match parse_scram_attrs(&server_first) {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "failed to parse SCRAM server-first");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "malformed SCRAM server-first").await;
                    return;
                }
            };

            let server_nonce = match attrs.get("r") {
                Some(v) => *v,
                None => {
                    warn!("SCRAM server-first missing r=");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "SCRAM server-first missing r=").await;
                    return;
                }
            };
            let salt_b64 = match attrs.get("s") {
                Some(v) => *v,
                None => {
                    warn!("SCRAM server-first missing s=");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "SCRAM server-first missing s=").await;
                    return;
                }
            };
            let iters_str = match attrs.get("i") {
                Some(v) => *v,
                None => {
                    warn!("SCRAM server-first missing i=");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "SCRAM server-first missing i=").await;
                    return;
                }
            };

            // Verify nonce starts with our client nonce.
            if !server_nonce.starts_with(hs.client_nonce.as_str()) {
                warn!("SCRAM server nonce does not start with client nonce");
                hs.step = SaslStep::Done;
                drop(guard);
                abort_sasl(ctx, "SCRAM nonce mismatch").await;
                return;
            }

            let salt = match BASE64.decode(salt_b64) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "SCRAM salt is not valid base64");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "invalid SCRAM salt").await;
                    return;
                }
            };
            let iterations: u32 = match iters_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    warn!("SCRAM iterations is not a valid number");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "invalid SCRAM iteration count").await;
                    return;
                }
            };

            let password = hs.password.clone();
            let client_first_bare = hs.client_first_bare.clone();
            let server_nonce = server_nonce.to_string();

            // Compute SCRAM key material (CPU-heavy: PBKDF2).
            // We offload to a blocking thread to avoid stalling the async executor.
            let (client_final, expected_server_sig, auth_message) =
                tokio::task::block_in_place(|| {
                    compute_scram_client_final(
                        &password,
                        &salt,
                        iterations,
                        &client_first_bare,
                        &server_first,
                        &server_nonce,
                    )
                });

            hs.auth_message = auth_message;
            hs.expected_server_sig = expected_server_sig;
            hs.step = SaslStep::ScramAwaitingServerFinal;
            drop(guard);

            debug!("sending SCRAM client-final");
            let encoded = BASE64.encode(client_final.as_bytes());
            let _ = ctx.line_tx.send(format!("AUTHENTICATE {encoded}")).await;
        }

        SaslStep::ScramAwaitingServerFinal => {
            // SCRAM step 3: server-final = v=<server_sig_b64>
            let raw = match BASE64.decode(payload) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "failed to decode SCRAM server-final");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "invalid base64 in SCRAM server-final").await;
                    return;
                }
            };
            let server_final = match std::str::from_utf8(&raw) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    warn!("SCRAM server-final is not valid UTF-8");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "SCRAM server-final not UTF-8").await;
                    return;
                }
            };

            // Parse: v=<base64-ServerSignature>
            let attrs = match parse_scram_attrs(&server_final) {
                Ok(a) => a,
                Err(e) => {
                    warn!(error = %e, "failed to parse SCRAM server-final");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "malformed SCRAM server-final").await;
                    return;
                }
            };

            let server_sig_b64 = match attrs.get("v") {
                Some(v) => *v,
                None => {
                    // Could be an error: e=<error-message>
                    let err = attrs.get("e").copied().unwrap_or("unknown error");
                    warn!(error = %err, "SCRAM server-final contains error");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, &format!("SCRAM server error: {err}")).await;
                    return;
                }
            };

            let server_sig = match BASE64.decode(server_sig_b64) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "SCRAM server signature is not valid base64");
                    hs.step = SaslStep::Done;
                    drop(guard);
                    abort_sasl(ctx, "invalid SCRAM server signature").await;
                    return;
                }
            };

            // Verify the server signature matches our expectation.
            if server_sig != hs.expected_server_sig {
                warn!("SCRAM server signature verification failed");
                hs.step = SaslStep::Done;
                drop(guard);
                abort_sasl(ctx, "SCRAM server signature mismatch").await;
                return;
            }

            hs.step = SaslStep::AwaitingSuccess;
            drop(guard);

            info!("SCRAM server signature verified");
            // Server will follow with 900/903.
        }

        _ => {
            // Out-of-order or already done — ignore.
            debug!(step = ?hs.step, "ignoring out-of-order AUTHENTICATE");
        }
    }
}

// ---------------------------------------------------------------------------
// SASL result numerics
// ---------------------------------------------------------------------------

/// 900 RPL_LOGGEDIN — server confirmed account identity.
pub async fn handle_logged_in(msg: &IrcMessage, ctx: &ConnContext) {
    // params: <nick> <nick>!<user>@<host> <account> :<message>
    let account = msg.params.get(2).cloned().unwrap_or_default();
    info!(account = %account, "SASL logged in");
    // Mark SASL as completed so the registration handler knows not to
    // fall back to NickServ IDENTIFY.
    ctx.state.set_sasl_logged_in().await;
    let _ = ctx.event_tx.send(IrcEvent::SaslLoggedIn { account }).await;
}

/// 903 RPL_SASLSUCCESS — SASL exchange complete.
pub async fn handle_sasl_success(_msg: &IrcMessage, ctx: &ConnContext) {
    info!("SASL authentication successful");
    let mut guard = ctx.sasl_state.lock().await;
    if let Some(ref mut hs) = *guard {
        hs.step = SaslStep::Done;
    }
    drop(guard);
    let _ = ctx.line_tx.send("CAP END".to_string()).await;
}

/// 904 ERR_SASLFAIL / 906 ERR_SASLABORTED — authentication failed.
pub async fn handle_sasl_failure(msg: &IrcMessage, ctx: &ConnContext) {
    let code = match &msg.command {
        airc_shared::Command::Numeric(n) => *n,
        _ => 0,
    };
    let reason = msg.params.last().cloned().unwrap_or_default();
    warn!(code = code, reason = %reason, "SASL authentication failed");
    {
        let mut guard = ctx.sasl_state.lock().await;
        if let Some(ref mut hs) = *guard {
            hs.step = SaslStep::Done;
        }
    }
    let _ = ctx
        .event_tx
        .send(IrcEvent::SaslFailed { code, reason })
        .await;
    // Still send CAP END so the server completes registration (even
    // though the caller may choose to disconnect).
    let _ = ctx.line_tx.send("CAP END".to_string()).await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Abort an in-progress SASL exchange and emit a failure event.
async fn abort_sasl(ctx: &ConnContext, reason: &str) {
    let _ = ctx.line_tx.send("AUTHENTICATE *".to_string()).await;
    let _ = ctx
        .event_tx
        .send(IrcEvent::SaslFailed {
            code: 0,
            reason: reason.to_string(),
        })
        .await;
    let _ = ctx.line_tx.send("CAP END".to_string()).await;
}

/// Generate a cryptographically random client nonce (18 bytes → 24 base64 chars).
fn generate_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut bytes);
    BASE64.encode(bytes)
}

/// Parse `key=value,key=value,...` into a map.  Values may contain `=`.
fn parse_scram_attrs(s: &str) -> Result<std::collections::HashMap<&str, &str>, String> {
    let mut map = std::collections::HashMap::new();
    for part in s.split(',') {
        let (k, v) = part
            .split_once('=')
            .ok_or_else(|| format!("bad SCRAM attribute '{part}'"))?;
        map.insert(k, v);
    }
    Ok(map)
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

/// Compute SaltedPassword = PBKDF2-HMAC-SHA-256(password, salt, iters, 32)
fn salted_password(password: &str, salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut output = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, iterations, &mut output);
    output
}

/// Compute the full SCRAM client-final-message and expected server signature.
///
/// Returns `(client_final_message, expected_server_sig, auth_message)`.
fn compute_scram_client_final(
    password: &str,
    salt: &[u8],
    iterations: u32,
    client_first_bare: &str,
    server_first: &str,
    server_nonce: &str,
) -> (String, Vec<u8>, String) {
    // SaltedPassword = PBKDF2(password, salt, iters, 32)
    let salted_pw = salted_password(password, salt, iterations);

    // ClientKey = HMAC(SaltedPassword, "Client Key")
    let client_key = hmac_sha256(&salted_pw, b"Client Key");
    // StoredKey = SHA-256(ClientKey)
    let stored_key = sha256(&client_key);
    // ServerKey = HMAC(SaltedPassword, "Server Key")
    let server_key = hmac_sha256(&salted_pw, b"Server Key");

    // client-final-message-without-proof
    // c = base64("n,,")  (channel binding: none)
    let channel_binding = BASE64.encode(b"n,,");
    let client_final_without_proof = format!("c={channel_binding},r={server_nonce}");

    // AuthMessage = client-first-bare + "," + server-first + "," + client-final-without-proof
    let auth_message = format!("{client_first_bare},{server_first},{client_final_without_proof}");

    // ClientSignature = HMAC(StoredKey, AuthMessage)
    let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());

    // ClientProof = ClientKey XOR ClientSignature
    let mut client_proof = [0u8; 32];
    for i in 0..32 {
        client_proof[i] = client_key[i] ^ client_signature[i];
    }

    // ServerSignature = HMAC(ServerKey, AuthMessage)
    let server_signature = hmac_sha256(&server_key, auth_message.as_bytes());

    // client-final-message = client-final-without-proof + ",p=" + base64(ClientProof)
    let client_final = format!(
        "{client_final_without_proof},p={}",
        BASE64.encode(client_proof)
    );

    (client_final, server_signature.to_vec(), auth_message)
}
