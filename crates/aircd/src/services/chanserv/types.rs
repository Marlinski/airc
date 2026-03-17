//! ChanServ data types — registered channel metadata.

use serde::{Deserialize, Serialize};

/// A registered channel's persistent metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredChannel {
    /// Channel name (canonical casing).
    pub name: String,
    /// Founder's nick (lowercase).
    pub founder: String,
    /// Minimum reputation required to join (0 = no requirement).
    pub min_reputation: i64,
    /// Banned nick patterns (lowercase).
    pub bans: Vec<String>,
    /// Description / purpose.
    pub description: Option<String>,
}
