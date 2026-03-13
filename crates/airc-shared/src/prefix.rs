//! Structured IRC message prefix (source identifier).
//!
//! An IRC prefix appears at the start of server-originated messages in the
//! form `:nick!user@host` or just `:servername`. This module provides a
//! typed representation that can be parsed from and formatted to the wire
//! format.

use std::fmt;

/// A parsed IRC message prefix (source identifier).
///
/// Can represent either a user (`nick!user@host`) or a server name.
///
/// # Examples
///
/// ```
/// use airc_shared::prefix::Prefix;
///
/// let p = Prefix::parse("nick!user@host.com");
/// assert_eq!(p.nick(), "nick");
/// assert_eq!(p.user(), Some("user"));
/// assert_eq!(p.host(), Some("host.com"));
///
/// let s = Prefix::parse("irc.server.com");
/// assert_eq!(s.nick(), "irc.server.com");
/// assert_eq!(s.user(), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    /// The full raw prefix string.
    raw: String,
    /// Byte offset of `!` if present (separates nick from user).
    bang: Option<usize>,
    /// Byte offset of `@` if present (separates user from host).
    at: Option<usize>,
}

impl Prefix {
    /// Parse a prefix string into its components.
    ///
    /// The input should **not** include the leading `:`.
    pub fn parse(s: &str) -> Self {
        let bang = s.find('!');
        let at = s.find('@');
        Prefix {
            raw: s.to_string(),
            bang,
            at,
        }
    }

    /// Build a user prefix from parts.
    pub fn user_prefix(nick: &str, user: &str, host: &str) -> Self {
        let raw = format!("{nick}!{user}@{host}");
        let bang = Some(nick.len());
        let at = Some(nick.len() + 1 + user.len());
        Prefix { raw, bang, at }
    }

    /// Build a server prefix.
    pub fn server(name: &str) -> Self {
        Prefix {
            raw: name.to_string(),
            bang: None,
            at: None,
        }
    }

    /// The nick portion (or the whole string if not a user prefix).
    pub fn nick(&self) -> &str {
        match self.bang {
            Some(pos) => &self.raw[..pos],
            None => match self.at {
                Some(pos) => &self.raw[..pos],
                None => &self.raw,
            },
        }
    }

    /// The username portion, if present.
    pub fn user(&self) -> Option<&str> {
        let bang = self.bang?;
        let end = self.at.unwrap_or(self.raw.len());
        Some(&self.raw[bang + 1..end])
    }

    /// The hostname portion, if present.
    pub fn host(&self) -> Option<&str> {
        let at = self.at?;
        Some(&self.raw[at + 1..])
    }

    /// The full raw prefix string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether this looks like a user prefix (has `!` and `@`).
    pub fn is_user(&self) -> bool {
        self.bang.is_some() && self.at.is_some()
    }

    /// Whether this looks like a server prefix (no `!` or `@`).
    pub fn is_server(&self) -> bool {
        self.bang.is_none() && self.at.is_none()
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl From<&str> for Prefix {
    fn from(s: &str) -> Self {
        Prefix::parse(s)
    }
}

impl From<String> for Prefix {
    fn from(s: String) -> Self {
        Prefix::parse(&s)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_user_prefix() {
        let p = Prefix::parse("nick!user@host.com");
        assert_eq!(p.nick(), "nick");
        assert_eq!(p.user(), Some("user"));
        assert_eq!(p.host(), Some("host.com"));
        assert!(p.is_user());
        assert!(!p.is_server());
    }

    #[test]
    fn parse_server_prefix() {
        let p = Prefix::parse("irc.server.com");
        assert_eq!(p.nick(), "irc.server.com");
        assert_eq!(p.user(), None);
        assert_eq!(p.host(), None);
        assert!(p.is_server());
        assert!(!p.is_user());
    }

    #[test]
    fn parse_nick_at_host_no_user() {
        let p = Prefix::parse("nick@host.com");
        assert_eq!(p.nick(), "nick");
        assert_eq!(p.user(), None);
        assert_eq!(p.host(), Some("host.com"));
    }

    #[test]
    fn display_roundtrip() {
        let raw = "nick!user@host.com";
        let p = Prefix::parse(raw);
        assert_eq!(p.to_string(), raw);
    }

    #[test]
    fn user_prefix_builder() {
        let p = Prefix::user_prefix("alice", "asmith", "example.com");
        assert_eq!(p.nick(), "alice");
        assert_eq!(p.user(), Some("asmith"));
        assert_eq!(p.host(), Some("example.com"));
        assert_eq!(p.to_string(), "alice!asmith@example.com");
    }
}
