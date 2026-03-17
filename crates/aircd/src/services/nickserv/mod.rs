//! NickServ — nickname registration and authentication service.
//!
//! Composed of togglable modules:
//! - **identity**: REGISTER, IDENTIFY, INFO, GHOST/RELEASE
//! - **keypair**: REGISTER-KEY, CHALLENGE, VERIFY
//! - **reputation**: VOUCH, REPORT, REPUTATION
//! - **social**: FRIEND (social graph)
//! - **silence**: SILENCE (client-side filtering)

// Public sub-modules (handlers).
pub mod identity;
pub mod keypair;
pub mod reputation;
pub mod silence;
pub mod social;

// Private implementation modules.
mod dispatch;
mod persist;
mod state;
mod types;

// Public re-exports consumed by services/mod.rs.
pub use dispatch::create_dispatcher;
pub use state::NickServState;

// Crate-internal re-exports consumed by sub-modules via `crate::services::nickserv::`.
pub(crate) use persist::{hex_decode, hex_encode, now_unix, parse_pubkey};
pub(crate) use types::{Identity, PendingChallenge};

// Re-exported for the SASL layer and any other crate-internal consumers.
pub use persist::hash_password;
