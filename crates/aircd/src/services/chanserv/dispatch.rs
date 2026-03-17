//! ChanServ module builder and dispatcher factory.

use std::sync::Arc;

use crate::services::ChanServModules;
use crate::services::module::{ServiceDispatcher, ServiceModule};
use crate::state::SharedState;

use super::state::ChanServState;
use super::{access, registration};

/// Build the set of ChanServ modules based on config toggles.
pub fn build_modules(
    state: Arc<ChanServState>,
    modules_cfg: &ChanServModules,
    shared: SharedState,
) -> Vec<Box<dyn ServiceModule>> {
    let mut modules: Vec<Box<dyn ServiceModule>> = Vec::new();

    if modules_cfg.registration {
        modules.push(Box::new(registration::RegistrationModule::new(
            state.clone(),
            shared,
        )));
    }
    if modules_cfg.access {
        modules.push(Box::new(access::AccessModule::new(state.clone())));
    }

    modules
}

/// Create a fully-wired ChanServ dispatcher.
pub fn create_dispatcher(
    state: Arc<ChanServState>,
    modules_cfg: &ChanServModules,
    shared: SharedState,
) -> ServiceDispatcher {
    let modules = build_modules(state, modules_cfg, shared);
    ServiceDispatcher::new("ChanServ", modules)
}
