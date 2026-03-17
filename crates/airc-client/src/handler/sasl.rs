//! SASL authentication handlers.
//!
//! Drives the `AUTHENTICATE` challenge/response exchange and handles the
//! SASL result numerics (900 RPL_LOGGEDIN, 903 RPL_SASLSUCCESS,
//! 904 ERR_SASLFAIL, 906 ERR_SASLABORTED).

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use tracing::{debug, info, warn};

use airc_shared::IrcMessage;

use crate::config::SaslMechanism;
use crate::event::IrcEvent;

use super::ConnContext;
use super::cap::SaslStep;

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

    if hs.step != SaslStep::AwaitingChallenge {
        // Out-of-order message — ignore.
        return;
    }

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
            raw.extend_from_slice(hs.account.as_bytes());
            raw.push(0);
            raw.extend_from_slice(hs.password.as_bytes());
            let encoded = BASE64.encode(&raw);

            hs.step = SaslStep::AwaitingSuccess;
            drop(guard);

            debug!("sending SASL PLAIN credentials");
            let _ = ctx.line_tx.send(format!("AUTHENTICATE {encoded}")).await;
        }

        SaslMechanism::ScramSha256 => {
            // SCRAM requires a multi-step exchange not yet implemented
            // on the client side. Abort gracefully.
            warn!("SCRAM-SHA-256 client-side not yet implemented, aborting SASL");
            hs.step = SaslStep::Done;
            drop(guard);
            let _ = ctx.line_tx.send("AUTHENTICATE *".to_string()).await;
            let _ = ctx
                .event_tx
                .send(IrcEvent::SaslFailed {
                    code: 0,
                    reason: "SCRAM-SHA-256 not yet supported by the client".to_string(),
                })
                .await;
            let _ = ctx.line_tx.send("CAP END".to_string()).await;
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
