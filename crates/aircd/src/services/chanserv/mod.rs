//! ChanServ — channel registration and access control service.
//!
//! Composed of togglable modules:
//! - **registration**: REGISTER, INFO, SET
//! - **access**: BAN, UNBAN (+ check_join for future use)

// Public sub-modules (handlers).
pub mod access;
pub mod registration;

// Private implementation modules.
mod dispatch;
mod persist;
mod state;
mod types;

// Public re-exports consumed by services/mod.rs and sub-modules.
pub use dispatch::create_dispatcher;
pub use state::ChanServState;
pub use types::RegisteredChannel;
