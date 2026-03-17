//! CAP negotiation handler.
//!
//! Owns the `SaslHandshake` state machine and drives the
//! `CAP LS` → `CAP REQ :sasl` → `CAP ACK` → `AUTHENTICATE` sequence.

use tracing::{debug, info, warn};

use airc_shared::IrcMessage;

use crate::config::SaslMechanism;
use crate::event::IrcEvent;

use super::ConnContext;

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
    pub account: String,
    pub password: String,
    pub mechanism: SaslMechanism,
    pub step: SaslStep,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SaslStep {
    /// Sent `CAP REQ :sasl`; waiting for `CAP * ACK :sasl`.
    AwaitingCapAck,
    /// Got `CAP ACK`; sent `AUTHENTICATE <MECH>`; waiting for `AUTHENTICATE +`.
    AwaitingChallenge,
    /// Sent credentials; waiting for 900/903 confirmation.
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
            let caps: Vec<&str> = caps_str.split_whitespace().collect();
            debug!(caps = ?caps, "server advertised capabilities");

            let wants_sasl = ctx.sasl_state.lock().await.is_some();

            if wants_sasl && caps.iter().any(|c| c.eq_ignore_ascii_case("sasl")) {
                info!("server supports sasl, requesting capability");
                let _ = ctx.line_tx.send("CAP REQ :sasl".to_string()).await;
            } else {
                if wants_sasl {
                    warn!("server does not advertise sasl capability, skipping SASL");
                }
                let _ = ctx.line_tx.send("CAP END".to_string()).await;
            }
        }

        // Server acknowledged our CAP REQ.
        Some("ACK") => {
            let acked = msg.params.last().cloned().unwrap_or_default();
            debug!(caps = %acked, "CAP ACK received");

            let mut guard = ctx.sasl_state.lock().await;
            if let Some(ref mut hs) = *guard {
                if hs.step == SaslStep::AwaitingCapAck
                    && acked.to_ascii_lowercase().contains("sasl")
                {
                    hs.step = SaslStep::AwaitingChallenge;
                    let mech = hs.mechanism.wire_name().to_string();
                    drop(guard);
                    info!(mechanism = %mech, "starting SASL authentication");
                    let _ = ctx.line_tx.send(format!("AUTHENTICATE {mech}")).await;
                    return;
                }
            }
            // Unrecognised ACK — just end CAP.
            let _ = ctx.line_tx.send("CAP END".to_string()).await;
        }

        // Server rejected our CAP REQ.
        Some("NAK") => {
            let nacked = msg.params.last().cloned().unwrap_or_default();
            warn!(caps = %nacked, "CAP NAK received, aborting SASL");
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
        }

        _ => {
            // Ignore DEL, NEW, LIST, unknown subcommands.
        }
    }
}
