//! CAP negotiation handler.
//!
//! Owns the `SaslHandshake` state machine and drives the
//! `CAP LS` → `CAP REQ :sasl` → `CAP ACK` → `AUTHENTICATE` sequence.
//!
//! # IRCv3 cap negotiation strategy
//!
//! 1. On CAP LS: send `CAP REQ :sasl` (if sasl is advertised) as the first
//!    request.  Simultaneously send a second `CAP REQ :<optional-caps>` for
//!    any of the supported IRCv3 caps the server advertises.
//! 2. On CAP ACK: if the acked set contains `sasl`, advance the SASL state
//!    machine.  If it contains other caps, store them in `negotiated_caps`.
//!    If `sasl` is NOT in the acked set (server acked only optional caps),
//!    send `CAP END` immediately.
//! 3. On CAP NAK: if it mentions `sasl`, abort SASL and send `CAP END`.
//!    If it is for the optional-cap batch, log and ignore (no effect on
//!    connection progress).

use tracing::{debug, info, warn};

use airc_shared::IrcMessage;

use crate::event::IrcEvent;

use super::ConnContext;

// ---------------------------------------------------------------------------
// Supported optional IRCv3 caps (requested after sasl, if server advertises)
// ---------------------------------------------------------------------------

/// Optional IRCv3 caps the client knows how to use.
/// These are requested in a second `CAP REQ` after the sasl request.
const OPTIONAL_CAPS: &[&str] = &[
    "message-tags",
    "server-time",
    "echo-message",
    "away-notify",
    "multi-prefix",
    "extended-join",
    "account-notify",
];

// ---------------------------------------------------------------------------
// Private SASL mechanism selection (not exported)
// ---------------------------------------------------------------------------

/// The SASL mechanism chosen during CAP LS negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaslMechanism {
    /// SCRAM-SHA-256 — challenge-response; credentials never sent in clear text.
    ScramSha256,
    /// PLAIN — sends credentials base64-encoded.
    Plain,
}

impl SaslMechanism {
    /// The wire name used in `AUTHENTICATE <name>`.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            SaslMechanism::ScramSha256 => "SCRAM-SHA-256",
            SaslMechanism::Plain => "PLAIN",
        }
    }
}

// ---------------------------------------------------------------------------
// SASL handshake state machine (owned here, referenced by sasl.rs)
// ---------------------------------------------------------------------------

/// Tracks the in-progress CAP/SASL exchange.
///
/// Lives behind an `Arc<Mutex<Option<SaslHandshake>>>` shared between
/// `connect()` (which populates it) and the handler functions (which drive
/// it step-by-step as server messages arrive).
#[derive(Debug)]
pub struct SaslHandshake {
    /// Account name used as authcid (= nick).
    pub nick: String,
    /// Password for authentication.
    pub password: String,
    /// Chosen mechanism (placeholder until resolved in CAP LS).
    pub mechanism: SaslMechanism,
    /// Current step in the exchange.
    pub step: SaslStep,
    // SCRAM-SHA-256 state kept across steps:
    /// Client-generated nonce (random base64).
    pub client_nonce: String,
    /// `client-first-message-bare` (without `n,,` gs2 header).
    pub client_first_bare: String,
    /// Full auth-message used for SCRAM proof/verification.
    pub auth_message: String,
    /// Expected server signature to verify in server-final.
    pub expected_server_sig: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaslStep {
    /// Sent `CAP REQ :sasl`; waiting for `CAP * ACK :sasl`.
    AwaitingCapAck,
    /// Got `CAP ACK`; sent `AUTHENTICATE <MECH>`; waiting for `AUTHENTICATE +`.
    AwaitingChallenge,
    /// SCRAM: sent client-first; waiting for server-first.
    ScramAwaitingServerFirst,
    /// SCRAM: sent client-final; waiting for server-final (verification).
    ScramAwaitingServerFinal,
    /// PLAIN: sent credentials; waiting for 900/903 confirmation.
    AwaitingSuccess,
    /// Exchange complete (success or failure). `CAP END` has been sent.
    Done,
}

// ---------------------------------------------------------------------------
// CAP handler
// ---------------------------------------------------------------------------

/// Handle a `CAP` message from the server.
pub async fn handle_cap(msg: &IrcMessage, ctx: &ConnContext) {
    // params: <nick/*/-> <subcommand> [<caps>]
    let subcommand = msg.params.get(1).map(|s| s.to_ascii_uppercase());

    match subcommand.as_deref() {
        // Server's capability list.
        Some("LS") => {
            let caps_str = msg.params.last().cloned().unwrap_or_default();
            debug!(caps = %caps_str, "server advertised capabilities");

            let wants_sasl = ctx.sasl_state.lock().await.is_some();

            // Find the `sasl` capability and parse the advertised mechanisms.
            // Servers may advertise: `sasl=PLAIN,SCRAM-SHA-256` or plain `sasl`.
            let sasl_mechanisms: Option<Vec<String>> =
                caps_str.split_whitespace().find_map(|cap| {
                    let lower = cap.to_ascii_lowercase();
                    if lower == "sasl" {
                        // No mechanism list — assume server supports at least PLAIN.
                        Some(vec!["PLAIN".to_string()])
                    } else if lower.starts_with("sasl=") {
                        let mechs = cap[5..]
                            .split(',')
                            .map(|m| m.to_ascii_uppercase())
                            .collect();
                        Some(mechs)
                    } else {
                        None
                    }
                });

            // Collect optional caps that the server advertises.
            let optional_caps_to_request: Vec<&str> = OPTIONAL_CAPS
                .iter()
                .filter(|&&cap| {
                    caps_str.split_whitespace().any(|advertised| {
                        // Server may advertise caps as "cap=value"; match on the
                        // name portion before any '='.
                        let name = advertised.split('=').next().unwrap_or(advertised);
                        name.eq_ignore_ascii_case(cap)
                    })
                })
                .copied()
                .collect();

            // --- Send sasl REQ (if desired and available) ---
            if wants_sasl {
                match sasl_mechanisms {
                    None => {
                        // Server does not advertise SASL — skip; NickServ fallback
                        // will fire after RPL_WELCOME in registration.rs.
                        warn!("server does not advertise sasl capability, skipping SASL");
                    }
                    Some(ref server_mechs) => {
                        // Pick the best available mechanism.
                        let chosen = if server_mechs.iter().any(|m| m == "SCRAM-SHA-256") {
                            SaslMechanism::ScramSha256
                        } else if server_mechs.iter().any(|m| m == "PLAIN") {
                            SaslMechanism::Plain
                        } else {
                            warn!(
                                mechs = ?server_mechs,
                                "server offers no supported SASL mechanism, skipping SASL"
                            );
                            // Fall through to optional caps + CAP END below.
                            send_optional_caps_and_end(ctx, &optional_caps_to_request).await;
                            return;
                        };

                        {
                            let mut guard = ctx.sasl_state.lock().await;
                            if let Some(ref mut hs) = *guard {
                                hs.mechanism = chosen;
                            }
                        }

                        info!(mechanism = %chosen.wire_name(), "server supports sasl, requesting capability");
                        let _ = ctx.line_tx.send("CAP REQ :sasl".to_string()).await;
                    }
                }
            }

            // If sasl was not wanted or not available, send optional caps now
            // (sasl case: optional caps are sent after sasl ACK).
            if !wants_sasl || sasl_mechanisms.is_none() {
                send_optional_caps_and_end(ctx, &optional_caps_to_request).await;
            } else if sasl_mechanisms.is_some() {
                // sasl REQ was sent; stash optional caps for after the sasl ACK.
                // We store them in negotiated_caps temporarily as a "pending" list
                // by encoding them into a side channel via a helper in ctx.
                // Simplest approach: send the optional REQ right now (IRCv3 allows
                // multiple concurrent REQ messages; each gets its own ACK/NAK).
                if !optional_caps_to_request.is_empty() {
                    let req = format!("CAP REQ :{}", optional_caps_to_request.join(" "));
                    debug!(req = %req, "requesting optional IRCv3 caps");
                    let _ = ctx.line_tx.send(req).await;
                }
            }
        }

        // Server acknowledged our CAP REQ.
        Some("ACK") => {
            let acked = msg.params.last().cloned().unwrap_or_default();
            debug!(caps = %acked, "CAP ACK received");

            // Parse all acked caps (space- or colon-prefixed list).
            let acked_caps: Vec<String> = acked
                .trim_start_matches(':')
                .split_whitespace()
                .map(|c| c.to_ascii_lowercase())
                .collect();

            let has_sasl = acked_caps.iter().any(|c| c == "sasl");

            // Store non-sasl caps in negotiated_caps.
            {
                let mut caps = ctx.negotiated_caps.write().await;
                for cap in &acked_caps {
                    if cap != "sasl" {
                        caps.insert(cap.clone());
                    }
                }
            }
            if !acked_caps.is_empty() {
                debug!(caps = ?acked_caps, "negotiated IRCv3 caps stored");
            }

            if has_sasl {
                let mut guard = ctx.sasl_state.lock().await;
                if let Some(ref mut hs) = *guard
                    && hs.step == SaslStep::AwaitingCapAck
                {
                    hs.step = SaslStep::AwaitingChallenge;
                    let mech = hs.mechanism.wire_name().to_string();
                    drop(guard);
                    info!(mechanism = %mech, "starting SASL authentication");
                    let _ = ctx.line_tx.send(format!("AUTHENTICATE {mech}")).await;
                    return;
                }
            }

            // No sasl in this ACK — either it's the optional-caps ACK, or an
            // unexpected ACK. Either way we do NOT send CAP END here — the SASL
            // flow will send it via sasl.rs after authentication completes (or
            // handle_cap LS sends it if sasl was never needed).
            //
            // Exception: if sasl state is None (no SASL configured) or Done,
            // and this was the only pending REQ, send CAP END.
            let sasl_guard = ctx.sasl_state.lock().await;
            let sasl_done = sasl_guard
                .as_ref()
                .is_none_or(|hs| hs.step == SaslStep::Done);
            drop(sasl_guard);

            if sasl_done {
                let _ = ctx.line_tx.send("CAP END".to_string()).await;
            }
            // Otherwise: sasl is in progress, CAP END will be sent by sasl.rs
            // after authentication finishes.
        }

        // Server rejected our CAP REQ.
        Some("NAK") => {
            let nacked = msg.params.last().cloned().unwrap_or_default();
            let nacked_lower = nacked.to_ascii_lowercase();

            if nacked_lower.contains("sasl") {
                // The critical sasl REQ was rejected.
                warn!(caps = %nacked, "CAP NAK for sasl, aborting SASL");
                {
                    let mut guard = ctx.sasl_state.lock().await;
                    if let Some(ref mut hs) = *guard {
                        hs.step = SaslStep::Done;
                    }
                }
                let _ = ctx
                    .event_tx
                    .send(IrcEvent::SaslFailed {
                        code: 0,
                        reason: format!("server rejected capability: {nacked}"),
                    })
                    .await;
                let _ = ctx.line_tx.send("CAP END".to_string()).await;
            } else {
                // Optional-caps REQ was rejected — not a problem.
                debug!(caps = %nacked, "CAP NAK for optional caps (ignored)");
            }
        }

        _ => {
            // Ignore DEL, NEW, LIST, unknown subcommands.
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send `CAP REQ :<caps>` for optional caps (if any), then `CAP END`.
async fn send_optional_caps_and_end(ctx: &ConnContext, caps: &[&str]) {
    if !caps.is_empty() {
        let req = format!("CAP REQ :{}", caps.join(" "));
        debug!(req = %req, "requesting optional IRCv3 caps");
        let _ = ctx.line_tx.send(req).await;
        // CAP END will be sent when the ACK arrives (handled in the ACK branch
        // above where sasl_done==true).
    } else {
        let _ = ctx.line_tx.send("CAP END".to_string()).await;
    }
}
