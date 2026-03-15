//! Embedded IRC services — NickServ and ChanServ running inside aircd.
//!
//! Phase A: Dispatch via direct function calls from handler.rs.
//! Phase B: CRDT-backed persistent state + SQLite write-through.

pub mod chanserv;
pub mod module;
pub mod nickserv;

use std::path::Path;
use std::sync::Arc;

use crate::client::ClientHandle;
use crate::config::ServicesConfig;
use crate::state::SharedState;

/// Top-level services state — holds both NickServ and ChanServ.
pub struct ServicesState {
    pub nickserv: Arc<nickserv::NickServState>,
    pub chanserv: Arc<chanserv::ChanServState>,
    ns_modules: NickServModules,
    cs_modules: ChanServModules,
}

/// Config-driven module toggles for NickServ.
pub struct NickServModules {
    pub identity: bool,
    pub keypair: bool,
    pub reputation: bool,
    pub social: bool,
    pub silence: bool,
}

impl Default for NickServModules {
    fn default() -> Self {
        Self { identity: true, keypair: true, reputation: true, social: true, silence: true }
    }
}

/// Config-driven module toggles for ChanServ.
pub struct ChanServModules {
    pub registration: bool,
    pub access: bool,
}

impl Default for ChanServModules {
    fn default() -> Self {
        Self { registration: true, access: true }
    }
}

impl ServicesState {
    /// Create and initialize all service state from config.
    pub fn new(config: &ServicesConfig, shared_state: SharedState, data_dir: &Path) -> Self {
        let ns_modules = NickServModules {
            identity: true,
            keypair: true,
            reputation: true,
            social: true,
            silence: true,
        };
        let cs_modules = ChanServModules {
            registration: true,
            access: true,
        };

        let ns_state = Arc::new(nickserv::NickServState::new(shared_state, data_dir));
        let cs_state = Arc::new(chanserv::ChanServState::new(data_dir));

        let _ = config; // config fields used for future toggles

        Self {
            nickserv: ns_state,
            chanserv: cs_state,
            ns_modules,
            cs_modules,
        }
    }

    /// Dispatch a PRIVMSG to NickServ.
    pub async fn dispatch_nickserv(&self, sender: &str, text: &str, client: &ClientHandle) {
        let dispatcher =
            nickserv::create_dispatcher(self.nickserv.clone(), &self.ns_modules);
        dispatcher.dispatch(sender, text, client).await;
    }

    /// Dispatch a PRIVMSG to ChanServ.
    pub async fn dispatch_chanserv(&self, sender: &str, text: &str, client: &ClientHandle) {
        let dispatcher =
            chanserv::create_dispatcher(self.chanserv.clone(), &self.cs_modules);
        dispatcher.dispatch(sender, text, client).await;
    }

    /// Check whether a user may join a channel (ChanServ access check).
    pub async fn check_join(&self, channel: &str, nick: &str) -> Result<(), String> {
        // Look up reputation from NickServ.
        let reputation = self
            .nickserv
            .get_identity(nick)
            .await
            .map(|id| id.reputation)
            .unwrap_or(0);
        self.chanserv.check_join(channel, nick, reputation).await
    }
}
