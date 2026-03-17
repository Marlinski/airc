//! Protocol message dispatch for the IRC client.
//!
//! This module owns the `ConnContext` that groups all per-connection shared
//! state, and exposes `handle_message()` as the single entry point called by
//! the transport layer (`conn::read_loop`).
//!
//! Sub-modules handle logically distinct parts of the IRC protocol:
//!
//! | Module           | Responsibility                                  |
//! |------------------|-------------------------------------------------|
//! | `cap`            | CAP LS / ACK / NAK, `SaslHandshake` state machine |
//! | `sasl`           | AUTHENTICATE challenge/response, PLAIN encoding |
//! | `registration`   | 001 RPL_WELCOME, 433 ERR_NICKNAMEINUSE, 332, 353 |
//! | `channel`        | JOIN, PART, QUIT, KICK, NICK, TOPIC             |
//! | `message`        | PRIVMSG, NOTICE, CTCP ACTION                    |
//! | `motd`           | 375, 372, 376                                   |

pub mod cap;
pub mod channel;
pub mod message;
pub mod motd;
pub mod registration;
pub mod sasl;

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};

use airc_shared::{Command, IrcMessage, Prefix};

use crate::conn::LineSender;
use crate::event::IrcEvent;
use crate::state::ClientState;

use self::cap::SaslHandshake;

// ---------------------------------------------------------------------------
// ConnContext — bundles per-connection shared state
// ---------------------------------------------------------------------------

/// All shared state needed by protocol handlers.
///
/// Constructed once per connection in `read_loop` and passed by reference
/// into `handle_message` and all sub-handlers.
pub struct ConnContext {
    pub line_tx: LineSender,
    pub event_tx: mpsc::Sender<IrcEvent>,
    pub state: ClientState,
    pub sasl_state: Arc<Mutex<Option<SaslHandshake>>>,
}

// ---------------------------------------------------------------------------
// Top-level dispatcher
// ---------------------------------------------------------------------------

/// Process a single parsed IRC message: update state, auto-respond, emit events.
pub async fn handle_message(msg: IrcMessage, ctx: &ConnContext) {
    use airc_shared::reply::*;

    match &msg.command {
        // -- Auto PONG --------------------------------------------------------
        Command::Ping => {
            let token = msg.params.first().map(|s| s.as_str()).unwrap_or("");
            let _ = ctx.line_tx.send(IrcMessage::pong(token).serialize()).await;
        }

        // -- CAP negotiation --------------------------------------------------
        Command::Cap => {
            cap::handle_cap(&msg, ctx).await;
        }

        // -- AUTHENTICATE challenge -------------------------------------------
        Command::Authenticate => {
            sasl::handle_authenticate(&msg, ctx).await;
        }

        // -- SASL success: RPL_LOGGEDIN (900) ---------------------------------
        Command::Numeric(n) if *n == RPL_LOGGEDIN => {
            sasl::handle_logged_in(&msg, ctx).await;
        }

        // -- SASL complete: RPL_SASLSUCCESS (903) ----------------------------
        Command::Numeric(n) if *n == RPL_SASLSUCCESS => {
            sasl::handle_sasl_success(&msg, ctx).await;
        }

        // -- SASL failure: ERR_SASLFAIL (904) or ERR_SASLABORTED (906) --------
        Command::Numeric(n) if *n == ERR_SASLFAIL || *n == ERR_SASLABORTED => {
            sasl::handle_sasl_failure(&msg, ctx).await;
        }

        // -- Registration complete: RPL_WELCOME (001) -------------------------
        Command::Numeric(n) if *n == RPL_WELCOME => {
            registration::handle_welcome(&msg, ctx).await;
        }

        // -- Nick in use (433) ------------------------------------------------
        Command::Numeric(n) if *n == ERR_NICKNAMEINUSE => {
            registration::handle_nick_in_use(&msg, ctx).await;
        }

        // -- Topic (332) ------------------------------------------------------
        Command::Numeric(n) if *n == RPL_TOPIC => {
            registration::handle_topic_reply(&msg, ctx).await;
        }

        // -- NAMES reply (353) ------------------------------------------------
        Command::Numeric(n) if *n == RPL_NAMREPLY => {
            registration::handle_names_reply(&msg, ctx).await;
        }

        // -- JOIN -------------------------------------------------------------
        Command::Join => {
            channel::handle_join(&msg, ctx).await;
        }

        // -- PART -------------------------------------------------------------
        Command::Part => {
            channel::handle_part(&msg, ctx).await;
        }

        // -- QUIT -------------------------------------------------------------
        Command::Quit => {
            channel::handle_quit(&msg, ctx).await;
        }

        // -- KICK -------------------------------------------------------------
        Command::Kick => {
            channel::handle_kick(&msg, ctx).await;
        }

        // -- NICK change ------------------------------------------------------
        Command::Nick => {
            channel::handle_nick(&msg, ctx).await;
        }

        // -- TOPIC change -----------------------------------------------------
        Command::Topic => {
            channel::handle_topic(&msg, ctx).await;
        }

        // -- PRIVMSG ----------------------------------------------------------
        Command::Privmsg => {
            message::handle_privmsg(&msg, ctx).await;
        }

        // -- NOTICE -----------------------------------------------------------
        Command::Notice => {
            message::handle_notice(&msg, ctx).await;
        }

        // -- MOTD (375, 372, 376) ---------------------------------------------
        Command::Numeric(n) if *n == RPL_MOTDSTART => {
            motd::handle_motd_start(&msg, ctx).await;
        }
        Command::Numeric(n) if *n == RPL_MOTD => {
            motd::handle_motd_line(&msg, ctx).await;
        }
        Command::Numeric(n) if *n == RPL_ENDOFMOTD => {
            motd::handle_motd_end(&msg, ctx).await;
        }

        // -- Everything else: emit as Raw ------------------------------------
        _ => {
            let _ = ctx
                .event_tx
                .send(IrcEvent::Raw {
                    line: msg.serialize(),
                })
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Extract the nick from an optional prefix string.
pub fn extract_nick(prefix: &Option<String>) -> String {
    match prefix {
        Some(p) => Prefix::parse(p).nick().to_string(),
        None => String::new(),
    }
}
