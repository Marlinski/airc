//! NickServ module builder and dispatcher factory.

use std::sync::Arc;

use crate::services::NickServModules;
use crate::services::module::{ServiceDispatcher, ServiceModule};

use super::state::NickServState;
use super::{identity, keypair, reputation, silence, social};

/// Build the set of NickServ modules based on config toggles.
pub fn build_modules(
    state: Arc<NickServState>,
    modules_cfg: &NickServModules,
) -> Vec<Box<dyn ServiceModule>> {
    let mut modules: Vec<Box<dyn ServiceModule>> = Vec::new();

    if modules_cfg.identity {
        modules.push(Box::new(identity::IdentityModule::new(state.clone())));
    }
    if modules_cfg.keypair {
        modules.push(Box::new(keypair::KeypairModule::new(state.clone())));
    }
    if modules_cfg.reputation {
        modules.push(Box::new(reputation::ReputationModule::new(state.clone())));
    }
    if modules_cfg.social {
        modules.push(Box::new(social::SocialModule::new(state.clone())));
    }
    if modules_cfg.silence {
        modules.push(Box::new(silence::SilenceModule::new(state.clone())));
    }

    modules
}

/// Create a fully-wired NickServ dispatcher.
pub fn create_dispatcher(
    state: Arc<NickServState>,
    modules_cfg: &NickServModules,
) -> ServiceDispatcher {
    let modules = build_modules(state, modules_cfg);
    ServiceDispatcher::new("NickServ", modules)
}
