//! ChanServ utilities — glob matching helper.
//!
//! All JSON persistence functions have been removed. Persistent state is
//! stored exclusively via the CRDT-backed `PersistentState` (SQLite).

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::util::glob_match;

    #[test]
    fn glob_exact() {
        assert!(glob_match("alice", "alice"));
        assert!(!glob_match("alice", "bob"));
    }

    #[test]
    fn glob_wildcard() {
        assert!(glob_match("*bot", "testbot"));
        assert!(glob_match("agent*", "agent42"));
        assert!(glob_match("*mid*", "in_middle_here"));
        assert!(!glob_match("*bot", "botnet"));
    }

    #[test]
    fn glob_star_only() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }
}
