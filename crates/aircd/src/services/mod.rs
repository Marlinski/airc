//! Service bot infrastructure.
//!
//! Service bots (NickServ, ChanServ) are virtual clients that respond to
//! `PRIVMSG`. They are not real TCP connections — they exist purely in code
//! and use the [`ServiceBot`] trait.
//!
//! The [`ServiceRouter`] checks every PRIVMSG/NOTICE and, if the target
//! matches a registered service, dispatches to it instead of doing normal
//! message routing.

pub mod chanserv;
pub mod nickserv;

use crate::client::ClientHandle;
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// ServiceBot trait
// ---------------------------------------------------------------------------

/// A virtual IRC service bot that handles commands sent via PRIVMSG.
pub trait ServiceBot: Send + Sync {
    /// The nickname of this service (e.g. "NickServ").
    fn nick(&self) -> &str;

    /// Handle a PRIVMSG directed at this service.
    /// `sender` is the client who sent the message, `text` is the message body.
    fn handle(
        &self,
        state: &SharedState,
        sender: &ClientHandle,
        text: &str,
    ) -> impl std::future::Future<Output = ()> + Send;
}

// ---------------------------------------------------------------------------
// ServiceRouter
// ---------------------------------------------------------------------------

/// Routes messages to service bots. Holds all registered services.
pub struct ServiceRouter {
    pub nickserv: nickserv::NickServ,
    pub chanserv: chanserv::ChanServ,
}

impl ServiceRouter {
    pub fn new() -> Self {
        Self {
            nickserv: nickserv::NickServ::new(),
            chanserv: chanserv::ChanServ::new(),
        }
    }

    /// Try to route a PRIVMSG to a service bot. Returns `true` if the target
    /// was a service (message handled), `false` if it should be routed normally.
    pub async fn try_route(
        &self,
        state: &SharedState,
        sender: &ClientHandle,
        target: &str,
        text: &str,
    ) -> bool {
        if target.eq_ignore_ascii_case(self.nickserv.nick()) {
            self.nickserv.handle(state, sender, text).await;
            return true;
        }
        if target.eq_ignore_ascii_case(self.chanserv.nick()) {
            self.chanserv.handle(state, sender, text).await;
            return true;
        }
        false
    }
}
