//! IRC protocol-level validation helpers.
//!
//! These functions implement the rules from RFC 2812 for nicknames,
//! channel names, and other protocol identifiers. They are pure functions
//! with no I/O, usable by both clients and servers.

// ---------------------------------------------------------------------------
// Nickname validation
// ---------------------------------------------------------------------------

/// Validate an IRC nickname per RFC 2812 (relaxed).
///
/// Rules:
/// - 1–30 characters (the RFC says 9, but most modern servers allow more).
/// - Must start with a letter or one of the "special" characters `[]\\`_^{|}`.
/// - Subsequent characters may be alphanumeric, hyphen, or special.
///
/// # Examples
///
/// ```
/// use airc_shared::validate::is_valid_nick;
/// assert!(is_valid_nick("alice"));
/// assert!(is_valid_nick("[bot]"));
/// assert!(!is_valid_nick(""));
/// assert!(!is_valid_nick("123bad"));
/// assert!(!is_valid_nick("#channel"));
/// ```
pub fn is_valid_nick(nick: &str) -> bool {
    if nick.is_empty() || nick.len() > 30 {
        return false;
    }
    let mut chars = nick.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && !"[]\\`_^{|}".contains(first) {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || "-[]\\`_^{|}".contains(c))
}

// ---------------------------------------------------------------------------
// Channel name validation
// ---------------------------------------------------------------------------

/// Whether a string looks like an IRC channel name.
///
/// Per RFC 2812, channels start with `#`, `&`, `+`, or `!`. In practice
/// almost all channels use `#` or `&`.
///
/// # Examples
///
/// ```
/// use airc_shared::validate::is_channel_name;
/// assert!(is_channel_name("#lobby"));
/// assert!(is_channel_name("&local"));
/// assert!(!is_channel_name("noprefix"));
/// assert!(!is_channel_name(""));
/// ```
pub fn is_channel_name(s: &str) -> bool {
    s.starts_with('#') || s.starts_with('&')
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- is_valid_nick -------------------------------------------------------

    #[test]
    fn valid_nicks() {
        assert!(is_valid_nick("alice"));
        assert!(is_valid_nick("Alice"));
        assert!(is_valid_nick("a"));
        assert!(is_valid_nick("agent-007"));
        assert!(is_valid_nick("[bot]"));
        assert!(is_valid_nick("_under_score"));
        assert!(is_valid_nick("a1234567890"));
    }

    #[test]
    fn invalid_nicks() {
        assert!(!is_valid_nick(""));
        assert!(!is_valid_nick("123bad"));
        assert!(!is_valid_nick("#channel"));
        assert!(!is_valid_nick("-dash"));
        assert!(!is_valid_nick("has space"));
        assert!(!is_valid_nick("a".repeat(31).as_str()));
    }

    // -- is_channel_name -----------------------------------------------------

    #[test]
    fn valid_channels() {
        assert!(is_channel_name("#lobby"));
        assert!(is_channel_name("#a"));
        assert!(is_channel_name("&local"));
        assert!(is_channel_name("#Agents-Hub"));
    }

    #[test]
    fn invalid_channels() {
        assert!(!is_channel_name(""));
        assert!(!is_channel_name("noprefix"));
        assert!(!is_channel_name("alice"));
    }
}
