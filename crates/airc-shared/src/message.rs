//! IRC message parsing and serialization per RFC 2812 with IRCv3 message-tags.
//!
//! An IRC message has the wire format:
//! ```text
//! [@tags] [:prefix] COMMAND [params...] [:trailing]\r\n
//! ```
//!
//! This module handles parsing raw lines (without the trailing `\r\n`) into
//! structured [`IrcMessage`] values, and serializing them back to wire format.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors that can occur when parsing a raw IRC line.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// The input line was completely empty.
    #[error("empty message")]
    Empty,

    /// A prefix was started with `:` but contained no value.
    #[error("empty prefix")]
    EmptyPrefix,

    /// No command was found after the optional prefix.
    #[error("missing command")]
    MissingCommand,
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

/// An IRC command — either a named command, a numeric reply, or an unknown string.
///
/// Named variants cover the standard commands needed by a typical IRC server.
/// Numeric replies (three-digit codes sent from server to client) are stored in
/// the [`Numeric`](Command::Numeric) variant. Anything else lands in
/// [`Unknown`](Command::Unknown).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    // -- Registration -------------------------------------------------------
    /// `NICK` — set or change nickname.
    Nick,
    /// `USER` — specify username, hostname, servername, and realname.
    User,
    /// `PASS` — connection password.
    Pass,
    /// `QUIT` — disconnect from the server.
    Quit,

    // -- Messaging ----------------------------------------------------------
    /// `PRIVMSG` — send a message to a user or channel.
    Privmsg,
    /// `NOTICE` — send a notice (no auto-reply expected).
    Notice,

    // -- Channels -----------------------------------------------------------
    /// `JOIN` — join one or more channels.
    Join,
    /// `PART` — leave one or more channels.
    Part,
    /// `KICK` — kick a user from a channel.
    Kick,
    /// `TOPIC` — view or set the topic of a channel.
    Topic,
    /// `MODE` — view or change user/channel modes.
    Mode,
    /// `INVITE` — invite a user to a channel.
    Invite,

    // -- Queries ------------------------------------------------------------
    /// `WHO` — query information about users.
    Who,
    /// `WHOIS` — query detailed information about a user.
    Whois,
    /// `LIST` — list channels and their topics.
    List,
    /// `NAMES` — list users visible on a channel.
    Names,
    /// `ISON` — check if a list of nicks are online.
    Ison,

    // -- Availability -------------------------------------------------------
    /// `AWAY` — set or clear away status.
    Away,
    /// `ACCOUNT` — IRCv3 account-notify: broadcast account name change.
    Account,

    // -- Moderation / social -------------------------------------------------
    /// `SILENCE` — manage the server-side silence list (+nick / -nick / list).
    Silence,
    /// `FRIEND` — manage the server-side friend list (+nick / -nick / list).
    Friend,

    // -- Operator -----------------------------------------------------------
    /// `OPER` — authenticate as an IRC operator.
    Oper,
    /// `KILL` — forcibly disconnect a user (operator/service command).
    Kill,

    // -- Capability negotiation ---------------------------------------------
    /// `CAP` — capability negotiation (IRCv3).
    ///
    /// The subcommand is the first parameter (`LS`, `LIST`, `REQ`, `ACK`,
    /// `NAK`, `END`). Additional parameters follow as normal params.
    Cap,
    /// `AUTHENTICATE` — SASL authentication exchange (IRCv3).
    Authenticate,

    // -- Server -------------------------------------------------------------
    /// `PING` — keepalive ping.
    Ping,
    /// `PONG` — keepalive pong.
    Pong,
    /// `MOTD` — request the Message of the Day.
    Motd,
    /// `VERSION` — request the server version.
    Version,

    // -- Catch-all ----------------------------------------------------------
    /// A three-digit numeric reply code (e.g. `001`, `433`).
    Numeric(u16),
    /// Any command string we don't explicitly handle.
    Unknown(String),
}

impl Command {
    /// Parse a command string (already uppercased by the caller) into a
    /// [`Command`] variant.
    ///
    /// Three-digit numeric strings become [`Command::Numeric`]; recognised
    /// command names map to their named variant; everything else becomes
    /// [`Command::Unknown`].
    pub fn from_str_upper(s: &str) -> Self {
        // Try numeric first — must be exactly three ASCII digits.
        if s.len() == 3 && s.bytes().all(|b| b.is_ascii_digit()) && let Ok(n) = s.parse::<u16>() {
            return Command::Numeric(n);
        }

        match s {
            "NICK" => Command::Nick,
            "USER" => Command::User,
            "PASS" => Command::Pass,
            "QUIT" => Command::Quit,
            "PRIVMSG" => Command::Privmsg,
            "NOTICE" => Command::Notice,
            "JOIN" => Command::Join,
            "PART" => Command::Part,
            "KICK" => Command::Kick,
            "TOPIC" => Command::Topic,
            "MODE" => Command::Mode,
            "INVITE" => Command::Invite,
            "WHO" => Command::Who,
            "WHOIS" => Command::Whois,
            "LIST" => Command::List,
            "NAMES" => Command::Names,
            "ISON" => Command::Ison,
            "AWAY" => Command::Away,
            "ACCOUNT" => Command::Account,
            "SILENCE" => Command::Silence,
            "FRIEND" => Command::Friend,
            "OPER" => Command::Oper,
            "KILL" => Command::Kill,
            "PING" => Command::Ping,
            "PONG" => Command::Pong,
            "MOTD" => Command::Motd,
            "VERSION" => Command::Version,
            "CAP" => Command::Cap,
            "AUTHENTICATE" => Command::Authenticate,
            _ => Command::Unknown(s.to_string()),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Nick => f.write_str("NICK"),
            Command::User => f.write_str("USER"),
            Command::Pass => f.write_str("PASS"),
            Command::Quit => f.write_str("QUIT"),
            Command::Privmsg => f.write_str("PRIVMSG"),
            Command::Notice => f.write_str("NOTICE"),
            Command::Join => f.write_str("JOIN"),
            Command::Part => f.write_str("PART"),
            Command::Kick => f.write_str("KICK"),
            Command::Topic => f.write_str("TOPIC"),
            Command::Mode => f.write_str("MODE"),
            Command::Invite => f.write_str("INVITE"),
            Command::Who => f.write_str("WHO"),
            Command::Whois => f.write_str("WHOIS"),
            Command::List => f.write_str("LIST"),
            Command::Names => f.write_str("NAMES"),
            Command::Ison => f.write_str("ISON"),
            Command::Away => f.write_str("AWAY"),
            Command::Account => f.write_str("ACCOUNT"),
            Command::Silence => f.write_str("SILENCE"),
            Command::Friend => f.write_str("FRIEND"),
            Command::Oper => f.write_str("OPER"),
            Command::Kill => f.write_str("KILL"),
            Command::Ping => f.write_str("PING"),
            Command::Pong => f.write_str("PONG"),
            Command::Motd => f.write_str("MOTD"),
            Command::Version => f.write_str("VERSION"),
            Command::Cap => f.write_str("CAP"),
            Command::Authenticate => f.write_str("AUTHENTICATE"),
            Command::Numeric(n) => write!(f, "{n:03}"),
            Command::Unknown(s) => f.write_str(s),
        }
    }
}

// ---------------------------------------------------------------------------
// IrcMessage
// ---------------------------------------------------------------------------

/// A parsed IRC protocol message.
///
/// # Examples
///
/// ```
/// use airc_shared::IrcMessage;
///
/// let msg = IrcMessage::parse(":server 001 nick :Welcome to IRC").unwrap();
/// assert_eq!(msg.prefix.as_deref(), Some("server"));
/// assert_eq!(msg.params.len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrcMessage {
    /// IRCv3 message tags. Empty for untagged messages.
    ///
    /// Each entry is `(key, value)` where value is `None` for boolean
    /// (value-less) tags.  Order is preserved and duplicates are allowed
    /// as the spec does not prohibit them.
    pub tags: Vec<(String, Option<String>)>,
    /// Optional message prefix (source). For server-originated messages this is
    /// typically `nick!user@host` or a server name.
    pub prefix: Option<String>,
    /// The parsed command.
    pub command: Command,
    /// The command parameters. The last parameter may have contained spaces
    /// (trailing parameter prefixed with `:`), but is stored without the
    /// leading colon.
    pub params: Vec<String>,
}

impl IrcMessage {
    // -- Parsing ------------------------------------------------------------

    /// Parse a raw IRC line into an [`IrcMessage`].
    ///
    /// The input should **not** contain the trailing `\r\n`.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if the line is empty or missing a command.
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let line = line.trim_end_matches(['\r', '\n']);

        if line.is_empty() {
            return Err(ParseError::Empty);
        }

        let mut rest = line;

        // --- tags (IRCv3) --------------------------------------------------
        let tags = if rest.starts_with('@') {
            let end = rest.find(' ').ok_or(ParseError::MissingCommand)?;
            let tags_block = &rest[1..end]; // strip leading '@'
            rest = &rest[end + 1..];
            rest = rest.trim_start();
            parse_tags(tags_block)
        } else {
            vec![]
        };

        // --- prefix --------------------------------------------------------
        let prefix = if rest.starts_with(':') {
            let end = rest.find(' ').ok_or(ParseError::MissingCommand)?;
            let pfx = &rest[1..end];
            if pfx.is_empty() {
                return Err(ParseError::EmptyPrefix);
            }
            rest = &rest[end + 1..];
            // Skip any extra spaces between prefix and command.
            rest = rest.trim_start();
            Some(pfx.to_string())
        } else {
            None
        };

        if rest.is_empty() {
            return Err(ParseError::MissingCommand);
        }

        // --- command -------------------------------------------------------
        let (cmd_str, remainder) = match rest.find(' ') {
            Some(pos) => (&rest[..pos], &rest[pos + 1..]),
            None => (rest, ""),
        };

        let command = Command::from_str_upper(&cmd_str.to_ascii_uppercase());

        // --- params --------------------------------------------------------
        let params = parse_params(remainder);

        Ok(IrcMessage {
            tags,
            prefix,
            command,
            params,
        })
    }

    // -- Serialization ------------------------------------------------------

    /// Serialize this message to IRC wire format **without** the trailing
    /// `\r\n`. The caller is responsible for appending `\r\n` before sending.
    pub fn serialize(&self) -> String {
        self.to_string()
    }

    // -- Builder / convenience constructors ---------------------------------

    /// Return a clone of this message with the given prefix set.
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = Some(prefix.into());
        self
    }

    /// Return a clone with the given tag added.
    ///
    /// `value` is `None` for boolean (value-less) tags.
    #[must_use]
    pub fn with_tag(mut self, key: impl Into<String>, value: Option<impl Into<String>>) -> Self {
        self.tags.push((key.into(), value.map(|v| v.into())));
        self
    }

    /// Create a `PRIVMSG` message.
    pub fn privmsg(target: &str, text: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Privmsg,
            params: vec![target.to_string(), text.to_string()],
        }
    }

    /// Create a `NOTICE` message.
    pub fn notice(target: &str, text: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Notice,
            params: vec![target.to_string(), text.to_string()],
        }
    }

    /// Create a `NICK` message.
    pub fn nick(nickname: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Nick,
            params: vec![nickname.to_string()],
        }
    }

    /// Create a `JOIN` message.
    pub fn join(channel: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Join,
            params: vec![channel.to_string()],
        }
    }

    /// Create a `PART` message with an optional reason.
    pub fn part(channel: &str, reason: Option<&str>) -> Self {
        let mut params = vec![channel.to_string()];
        if let Some(r) = reason {
            params.push(r.to_string());
        }
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Part,
            params,
        }
    }

    /// Create a `QUIT` message with an optional reason.
    pub fn quit(reason: Option<&str>) -> Self {
        let params = match reason {
            Some(r) => vec![r.to_string()],
            None => vec![],
        };
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Quit,
            params,
        }
    }

    /// Create a `PING` message.
    pub fn ping(token: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Ping,
            params: vec![token.to_string()],
        }
    }

    /// Create a `PONG` message.
    pub fn pong(token: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Pong,
            params: vec![token.to_string()],
        }
    }

    /// Create a `USER` message.
    ///
    /// ```
    /// use airc_shared::IrcMessage;
    /// let msg = IrcMessage::user("alice", "Alice Smith");
    /// assert_eq!(msg.serialize(), "USER alice 0 * :Alice Smith");
    /// ```
    pub fn user(username: &str, realname: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::User,
            params: vec![
                username.to_string(),
                "0".to_string(),
                "*".to_string(),
                realname.to_string(),
            ],
        }
    }

    /// Create a `PASS` message.
    pub fn pass(password: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Pass,
            params: vec![password.to_string()],
        }
    }

    /// Create an `OPER` message.
    ///
    /// ```
    /// use airc_shared::IrcMessage;
    /// let msg = IrcMessage::oper("admin", "secret");
    /// assert_eq!(msg.serialize(), "OPER admin secret");
    /// ```
    pub fn oper(name: &str, password: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Oper,
            params: vec![name.to_string(), password.to_string()],
        }
    }

    /// Create a `KILL` message.
    ///
    /// `KILL <nick> :<reason>` — forcibly disconnect a user.
    ///
    /// # Examples
    ///
    /// ```
    /// use airc_shared::IrcMessage;
    ///
    /// let msg = IrcMessage::kill("baduser", "Spamming the channel");
    /// assert_eq!(msg.serialize(), "KILL baduser :Spamming the channel");
    /// ```
    pub fn kill(nick: &str, reason: &str) -> Self {
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Kill,
            params: vec![nick.to_string(), reason.to_string()],
        }
    }

    /// Create a `MODE` message.
    pub fn mode(target: &str, modes: Option<&str>) -> Self {
        let mut params = vec![target.to_string()];
        if let Some(m) = modes {
            params.push(m.to_string());
        }
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Mode,
            params,
        }
    }

    /// Create a numeric reply message.
    ///
    /// `target` is typically the recipient's nickname. `params` are the
    /// remaining parameters for the numeric.
    pub fn numeric(code: u16, target: &str, params: &[&str]) -> Self {
        let mut p = vec![target.to_string()];
        p.extend(params.iter().map(|s| s.to_string()));
        IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Numeric(code),
            params: p,
        }
    }
}

// -- Display (wire format) --------------------------------------------------

impl fmt::Display for IrcMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Tags (IRCv3)
        if !self.tags.is_empty() {
            f.write_str("@")?;
            for (i, (key, value)) in self.tags.iter().enumerate() {
                if i > 0 {
                    f.write_str(";")?;
                }
                f.write_str(key)?;
                if let Some(val) = value {
                    f.write_str("=")?;
                    write!(f, "{}", escape_tag_value(val))?;
                }
            }
            f.write_str(" ")?;
        }

        // Prefix
        if let Some(ref pfx) = self.prefix {
            write!(f, ":{pfx} ")?;
        }

        // Command
        write!(f, "{}", self.command)?;

        // Parameters
        let len = self.params.len();
        for (i, param) in self.params.iter().enumerate() {
            let is_last = i + 1 == len;
            // The last parameter gets a `:` prefix if it contains spaces or
            // is empty (to preserve it on the wire), or if it starts with `:`.
            if is_last && (param.contains(' ') || param.is_empty() || param.starts_with(':')) {
                write!(f, " :{param}")?;
            } else {
                write!(f, " {param}")?;
            }
        }

        Ok(())
    }
}

// -- Serde (serialize as IRC wire string) -----------------------------------

impl Serialize for IrcMessage {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for IrcMessage {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        IrcMessage::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse the tags block (the part after `@` and before the first space) into
/// a list of `(key, Option<value>)` pairs.
///
/// Tags are separated by `;`. Each tag is either `key=value` or bare `key`.
/// Values are unescaped per the IRCv3 spec.
fn parse_tags(block: &str) -> Vec<(String, Option<String>)> {
    let mut tags = Vec::new();
    for token in block.split(';') {
        if token.is_empty() {
            continue;
        }
        match token.find('=') {
            Some(eq) => {
                let key = token[..eq].to_string();
                let raw_value = &token[eq + 1..];
                tags.push((key, Some(unescape_tag_value(raw_value))));
            }
            None => {
                tags.push((token.to_string(), None));
            }
        }
    }
    tags
}

/// Unescape a tag value per the IRCv3 message-tags spec.
///
/// Escape sequences:
/// - `\:` → `;`
/// - `\s` → ` `
/// - `\\` → `\`
/// - `\r` → CR
/// - `\n` → LF
/// - Any other `\X` → `X` (strip backslash)
fn unescape_tag_value(raw: &str) -> String {
    let mut result = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(':') => result.push(';'),
                Some('s') => result.push(' '),
                Some('\\') => result.push('\\'),
                Some('r') => result.push('\r'),
                Some('n') => result.push('\n'),
                Some(other) => result.push(other),
                None => {} // trailing backslash — drop it
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// A helper that returns an escaped representation of a tag value.
///
/// Characters that must be escaped per the IRCv3 spec:
/// - `;`  → `\:`
/// - ` `  → `\s`
/// - `\`  → `\\`
/// - CR   → `\r`
/// - LF   → `\n`
fn escape_tag_value(value: &str) -> impl fmt::Display + '_ {
    struct Escaped<'a>(&'a str);

    impl fmt::Display for Escaped<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for ch in self.0.chars() {
                match ch {
                    ';' => f.write_str("\\:")?,
                    ' ' => f.write_str("\\s")?,
                    '\\' => f.write_str("\\\\")?,
                    '\r' => f.write_str("\\r")?,
                    '\n' => f.write_str("\\n")?,
                    other => write!(f, "{other}")?,
                }
            }
            Ok(())
        }
    }

    Escaped(value)
}

/// Parse the parameter portion of an IRC message into a `Vec<String>`.
///
/// Parameters are space-separated. A parameter starting with `:` begins the
/// "trailing" parameter which consumes the rest of the line (and may contain
/// spaces). The leading `:` is stripped from the trailing value.
fn parse_params(input: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut rest = input;

    while !rest.is_empty() {
        // Trailing parameter — everything after the `:` is one parameter.
        if let Some(trailing) = rest.strip_prefix(':') {
            params.push(trailing.to_string());
            break;
        }

        match rest.find(' ') {
            Some(pos) => {
                let param = &rest[..pos];
                if !param.is_empty() {
                    params.push(param.to_string());
                }
                rest = &rest[pos + 1..];
                // Skip consecutive spaces (lenient).
                rest = rest.trim_start();
            }
            None => {
                // Last parameter, no trailing colon.
                params.push(rest.to_string());
                break;
            }
        }
    }

    params
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Parsing tests ------------------------------------------------------

    #[test]
    fn parse_simple_command() {
        let msg = IrcMessage::parse("QUIT").unwrap();
        assert_eq!(msg.tags, vec![]);
        assert_eq!(msg.prefix, None);
        assert_eq!(msg.command, Command::Quit);
        assert!(msg.params.is_empty());
    }

    #[test]
    fn parse_command_with_params() {
        let msg = IrcMessage::parse("NICK alice").unwrap();
        assert_eq!(msg.tags, vec![]);
        assert_eq!(msg.command, Command::Nick);
        assert_eq!(msg.params, vec!["alice"]);
    }

    #[test]
    fn parse_prefix_and_trailing() {
        let msg = IrcMessage::parse(":nick!user@host PRIVMSG #chan :hello world").unwrap();
        assert_eq!(msg.tags, vec![]);
        assert_eq!(msg.prefix.as_deref(), Some("nick!user@host"));
        assert_eq!(msg.command, Command::Privmsg);
        assert_eq!(msg.params, vec!["#chan", "hello world"]);
    }

    #[test]
    fn parse_numeric_reply() {
        let msg = IrcMessage::parse(":server 001 nick :Welcome to the IRC Network").unwrap();
        assert_eq!(msg.tags, vec![]);
        assert_eq!(msg.prefix.as_deref(), Some("server"));
        assert_eq!(msg.command, Command::Numeric(1));
        assert_eq!(msg.params, vec!["nick", "Welcome to the IRC Network"]);
    }

    #[test]
    fn parse_case_insensitive_command() {
        let msg = IrcMessage::parse("privmsg #test :hi").unwrap();
        assert_eq!(msg.command, Command::Privmsg);
    }

    #[test]
    fn parse_no_params() {
        let msg = IrcMessage::parse(":server PING").unwrap();
        assert_eq!(msg.tags, vec![]);
        assert_eq!(msg.prefix.as_deref(), Some("server"));
        assert_eq!(msg.command, Command::Ping);
        assert!(msg.params.is_empty());
    }

    #[test]
    fn parse_only_trailing() {
        let msg = IrcMessage::parse("QUIT :Gone for lunch").unwrap();
        assert_eq!(msg.command, Command::Quit);
        assert_eq!(msg.params, vec!["Gone for lunch"]);
    }

    #[test]
    fn parse_user_command() {
        let msg = IrcMessage::parse("USER alice 0 * :Alice Smith").unwrap();
        assert_eq!(msg.command, Command::User);
        assert_eq!(msg.params, vec!["alice", "0", "*", "Alice Smith"]);
    }

    #[test]
    fn parse_empty_trailing() {
        let msg = IrcMessage::parse("TOPIC #chan :").unwrap();
        assert_eq!(msg.command, Command::Topic);
        assert_eq!(msg.params, vec!["#chan", ""]);
    }

    #[test]
    fn parse_unknown_command() {
        let msg = IrcMessage::parse("FOOBAR arg1 arg2").unwrap();
        assert_eq!(msg.command, Command::Unknown("FOOBAR".to_string()));
        assert_eq!(msg.params, vec!["arg1", "arg2"]);
    }

    #[test]
    fn parse_strips_crlf() {
        let msg = IrcMessage::parse("PING server\r\n").unwrap();
        assert_eq!(msg.command, Command::Ping);
        assert_eq!(msg.params, vec!["server"]);
    }

    #[test]
    fn parse_empty_line() {
        assert_eq!(IrcMessage::parse(""), Err(ParseError::Empty));
    }

    #[test]
    fn parse_empty_prefix() {
        assert_eq!(
            IrcMessage::parse(": NICK alice"),
            Err(ParseError::EmptyPrefix)
        );
    }

    #[test]
    fn parse_prefix_only() {
        // A prefix with no command after it.
        assert_eq!(
            IrcMessage::parse(":server"),
            Err(ParseError::MissingCommand)
        );
    }

    #[test]
    fn parse_prefix_with_trailing_space_no_command() {
        assert_eq!(
            IrcMessage::parse(":server "),
            Err(ParseError::MissingCommand)
        );
    }

    #[test]
    fn parse_join_multiple_channels() {
        let msg = IrcMessage::parse("JOIN #a,#b,#c").unwrap();
        assert_eq!(msg.command, Command::Join);
        assert_eq!(msg.params, vec!["#a,#b,#c"]);
    }

    #[test]
    fn parse_kick_with_reason() {
        let msg = IrcMessage::parse(":op!u@h KICK #chan victim :You have been kicked").unwrap();
        assert_eq!(msg.command, Command::Kick);
        assert_eq!(msg.params, vec!["#chan", "victim", "You have been kicked"]);
    }

    #[test]
    fn parse_mode_command() {
        let msg = IrcMessage::parse("MODE #chan +o alice").unwrap();
        assert_eq!(msg.command, Command::Mode);
        assert_eq!(msg.params, vec!["#chan", "+o", "alice"]);
    }

    #[test]
    fn parse_multiple_spaces_lenient() {
        // Extra spaces between params — be lenient.
        let msg = IrcMessage::parse("NICK   alice").unwrap();
        assert_eq!(msg.command, Command::Nick);
        assert_eq!(msg.params, vec!["alice"]);
    }

    // -- Serialization tests ------------------------------------------------

    #[test]
    fn serialize_simple() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Quit,
            params: vec![],
        };
        assert_eq!(msg.serialize(), "QUIT");
    }

    #[test]
    fn serialize_with_prefix() {
        let msg = IrcMessage::privmsg("#chan", "hello world").with_prefix("nick!user@host");
        assert_eq!(
            msg.serialize(),
            ":nick!user@host PRIVMSG #chan :hello world"
        );
    }

    #[test]
    fn serialize_no_trailing_needed() {
        let msg = IrcMessage::nick("alice");
        assert_eq!(msg.serialize(), "NICK alice");
    }

    #[test]
    fn serialize_numeric() {
        let msg = IrcMessage::numeric(1, "nick", &["Welcome to IRC"]).with_prefix("server");
        assert_eq!(msg.serialize(), ":server 001 nick :Welcome to IRC");
    }

    #[test]
    fn serialize_numeric_padded() {
        // Numeric codes < 100 should be zero-padded to 3 digits.
        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Numeric(42),
            params: vec![],
        };
        assert_eq!(msg.serialize(), "042");
    }

    #[test]
    fn serialize_empty_trailing() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Topic,
            params: vec!["#chan".to_string(), "".to_string()],
        };
        assert_eq!(msg.serialize(), "TOPIC #chan :");
    }

    #[test]
    fn serialize_trailing_starts_with_colon() {
        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: Command::Privmsg,
            params: vec!["#chan".to_string(), ":)".to_string()],
        };
        assert_eq!(msg.serialize(), "PRIVMSG #chan ::)");
    }

    // -- Round-trip tests ---------------------------------------------------

    #[test]
    fn roundtrip_privmsg() {
        let raw = ":nick!user@host PRIVMSG #channel :hello world";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_numeric() {
        let raw = ":irc.server.com 433 * alice :Nickname is already in use";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_quit_no_params() {
        let raw = "QUIT";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_quit_with_reason() {
        let raw = "QUIT :Gone for lunch";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_user() {
        let raw = "USER alice 0 * :Alice Smith";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    // -- Builder / convenience tests ----------------------------------------

    #[test]
    fn builder_privmsg() {
        let msg = IrcMessage::privmsg("#test", "hi there");
        assert_eq!(msg.command, Command::Privmsg);
        assert_eq!(msg.params, vec!["#test", "hi there"]);
        assert_eq!(msg.prefix, None);
        assert_eq!(msg.tags, vec![]);
    }

    #[test]
    fn builder_with_prefix() {
        let msg = IrcMessage::ping("token").with_prefix("server.example.com");
        assert_eq!(msg.prefix.as_deref(), Some("server.example.com"));
        assert_eq!(msg.command, Command::Ping);
    }

    #[test]
    fn builder_numeric() {
        let msg = IrcMessage::numeric(353, "nick", &["= #chan", "alice bob"]);
        assert_eq!(msg.command, Command::Numeric(353));
        assert_eq!(msg.params, vec!["nick", "= #chan", "alice bob"]);
    }

    #[test]
    fn builder_part_with_reason() {
        let msg = IrcMessage::part("#chan", Some("Leaving"));
        assert_eq!(msg.params, vec!["#chan", "Leaving"]);
    }

    #[test]
    fn builder_part_without_reason() {
        let msg = IrcMessage::part("#chan", None);
        assert_eq!(msg.params, vec!["#chan"]);
    }

    #[test]
    fn builder_quit_with_reason() {
        let msg = IrcMessage::quit(Some("bye"));
        assert_eq!(msg.params, vec!["bye"]);
    }

    #[test]
    fn builder_quit_without_reason() {
        let msg = IrcMessage::quit(None);
        assert!(msg.params.is_empty());
    }

    // -- Command Display tests ----------------------------------------------

    #[test]
    fn command_display() {
        assert_eq!(Command::Nick.to_string(), "NICK");
        assert_eq!(Command::Numeric(1).to_string(), "001");
        assert_eq!(Command::Numeric(433).to_string(), "433");
        assert_eq!(Command::Unknown("FOO".into()).to_string(), "FOO");
    }

    // -- IRCv3 message-tags tests -------------------------------------------

    #[test]
    fn parse_tags_key_value() {
        let msg = IrcMessage::parse("@time=2023-01-01T00:00:00.000Z PRIVMSG #chan :hello").unwrap();
        assert_eq!(
            msg.tags,
            vec![(
                "time".to_string(),
                Some("2023-01-01T00:00:00.000Z".to_string())
            )]
        );
        assert_eq!(msg.prefix, None);
        assert_eq!(msg.command, Command::Privmsg);
        assert_eq!(msg.params, vec!["#chan", "hello"]);
    }

    #[test]
    fn parse_tags_bare_key() {
        let msg = IrcMessage::parse("@draft/typing PRIVMSG #chan :hi").unwrap();
        assert_eq!(msg.tags, vec![("draft/typing".to_string(), None)]);
    }

    #[test]
    fn parse_tags_multiple() {
        let msg = IrcMessage::parse("@tag1=value1;tag2;tag3=value3 PING token").unwrap();
        assert_eq!(
            msg.tags,
            vec![
                ("tag1".to_string(), Some("value1".to_string())),
                ("tag2".to_string(), None),
                ("tag3".to_string(), Some("value3".to_string())),
            ]
        );
        assert_eq!(msg.command, Command::Ping);
    }

    #[test]
    fn parse_tags_with_prefix() {
        let msg = IrcMessage::parse(
            "@time=2023-01-01T00:00:00.000Z;account=alice :alice!u@h PRIVMSG #chan :hello",
        )
        .unwrap();
        assert_eq!(
            msg.tags,
            vec![
                (
                    "time".to_string(),
                    Some("2023-01-01T00:00:00.000Z".to_string())
                ),
                ("account".to_string(), Some("alice".to_string())),
            ]
        );
        assert_eq!(msg.prefix.as_deref(), Some("alice!u@h"));
        assert_eq!(msg.command, Command::Privmsg);
        assert_eq!(msg.params, vec!["#chan", "hello"]);
    }

    #[test]
    fn parse_tags_no_prefix() {
        // Tags present, no prefix — valid per spec.
        let msg = IrcMessage::parse("@tag=val PRIVMSG #chan :hello").unwrap();
        assert_eq!(msg.tags, vec![("tag".to_string(), Some("val".to_string()))]);
        assert_eq!(msg.prefix, None);
        assert_eq!(msg.command, Command::Privmsg);
    }

    #[test]
    fn parse_tags_unescape_space() {
        let msg = IrcMessage::parse("@label=hello\\sworld PING x").unwrap();
        assert_eq!(
            msg.tags,
            vec![("label".to_string(), Some("hello world".to_string()))]
        );
    }

    #[test]
    fn parse_tags_unescape_semicolon() {
        let msg = IrcMessage::parse("@label=a\\:b PING x").unwrap();
        assert_eq!(
            msg.tags,
            vec![("label".to_string(), Some("a;b".to_string()))]
        );
    }

    #[test]
    fn parse_tags_unescape_backslash() {
        let msg = IrcMessage::parse("@label=a\\\\b PING x").unwrap();
        assert_eq!(
            msg.tags,
            vec![("label".to_string(), Some("a\\b".to_string()))]
        );
    }

    #[test]
    fn parse_tags_unescape_cr_lf() {
        let msg = IrcMessage::parse("@label=a\\r\\nb PING x").unwrap();
        assert_eq!(
            msg.tags,
            vec![("label".to_string(), Some("a\r\nb".to_string()))]
        );
    }

    #[test]
    fn parse_tags_unescape_unknown_escape() {
        // Any other `\X` → `X`
        let msg = IrcMessage::parse("@label=a\\zb PING x").unwrap();
        assert_eq!(
            msg.tags,
            vec![("label".to_string(), Some("azb".to_string()))]
        );
    }

    #[test]
    fn serialize_tags_key_value() {
        let msg =
            IrcMessage::privmsg("#chan", "hi").with_tag("time", Some("2023-01-01T00:00:00.000Z"));
        assert_eq!(
            msg.serialize(),
            "@time=2023-01-01T00:00:00.000Z PRIVMSG #chan hi"
        );
    }

    #[test]
    fn serialize_tags_bare_key() {
        let msg = IrcMessage::privmsg("#chan", "hi").with_tag("msgid", None::<String>);
        assert_eq!(msg.serialize(), "@msgid PRIVMSG #chan hi");
    }

    #[test]
    fn serialize_tags_multiple() {
        let msg = IrcMessage::privmsg("#chan", "hi")
            .with_tag("tag1", Some("value1"))
            .with_tag("tag2", None::<String>)
            .with_tag("tag3", Some("value3"));
        assert_eq!(
            msg.serialize(),
            "@tag1=value1;tag2;tag3=value3 PRIVMSG #chan hi"
        );
    }

    #[test]
    fn serialize_tags_escape_semicolon() {
        let msg = IrcMessage::ping("x").with_tag("label", Some("a;b"));
        assert_eq!(msg.serialize(), "@label=a\\:b PING x");
    }

    #[test]
    fn serialize_tags_escape_space() {
        let msg = IrcMessage::ping("x").with_tag("label", Some("hello world"));
        assert_eq!(msg.serialize(), "@label=hello\\sworld PING x");
    }

    #[test]
    fn serialize_tags_escape_backslash() {
        let msg = IrcMessage::ping("x").with_tag("label", Some("a\\b"));
        assert_eq!(msg.serialize(), "@label=a\\\\b PING x");
    }

    #[test]
    fn roundtrip_tags_with_prefix() {
        let raw =
            "@time=2023-01-01T00:00:00.000Z;account=alice :alice!u@h PRIVMSG #chan :hello world";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_tags_no_prefix() {
        let raw = "@tag=val PRIVMSG #chan :hello world";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_tags_bare_key() {
        let raw = "@draft/typing PRIVMSG #chan :hello world";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn roundtrip_tags_escaped_values() {
        // Values containing special chars survive a round-trip.
        let raw = "@label=hello\\sworld;x=a\\:b PING token";
        let msg = IrcMessage::parse(raw).unwrap();
        assert_eq!(msg.serialize(), raw);
    }

    #[test]
    fn with_tag_builder() {
        let msg = IrcMessage::privmsg("#chan", "hi")
            .with_tag("time", Some("2023"))
            .with_tag("bare", None::<String>);
        assert_eq!(
            msg.tags,
            vec![
                ("time".to_string(), Some("2023".to_string())),
                ("bare".to_string(), None),
            ]
        );
    }
}
