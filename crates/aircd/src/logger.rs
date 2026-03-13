//! Append-only channel logger — one CSV file per channel or DM.
//!
//! Delegates to [`airc_shared::log::FileLogger`] for all file I/O. This module
//! adds server-specific tracing on construction and provides the
//! `ChannelLogger` type expected by the rest of the server crate.

use std::path::PathBuf;

use airc_shared::log::FileLogger;
use tracing::{debug, warn};

/// Server-side channel logger.
///
/// Thin wrapper around the shared [`FileLogger`]. When `log_dir` is `None`,
/// all calls are no-ops.
pub struct ChannelLogger {
    inner: FileLogger,
}

impl ChannelLogger {
    /// Create a logger. If `log_dir` is `None`, logging is disabled (all
    /// calls are no-ops).
    pub fn new(log_dir: Option<PathBuf>) -> Self {
        if let Some(ref dir) = log_dir {
            if std::fs::create_dir_all(dir).is_err() {
                warn!(dir = %dir.display(), "cannot create log directory, logging disabled");
                return Self {
                    inner: FileLogger::new(None),
                };
            }
            debug!(dir = %dir.display(), "channel logging enabled");
        }
        Self {
            inner: FileLogger::new(log_dir),
        }
    }

    /// Convenience: log a message (PRIVMSG).
    pub fn log_message(&self, channel: &str, nick: &str, text: &str) {
        self.inner.log_message(channel, nick, text);
    }

    /// Convenience: log a NOTICE.
    pub fn log_notice(&self, channel: &str, nick: &str, text: &str) {
        self.inner.log_notice(channel, nick, text);
    }

    /// Convenience: log a JOIN.
    pub fn log_join(&self, channel: &str, nick: &str) {
        self.inner.log_join(channel, nick);
    }

    /// Convenience: log a PART.
    pub fn log_part(&self, channel: &str, nick: &str, reason: &str) {
        self.inner.log_part(channel, nick, reason);
    }

    /// Convenience: log a QUIT (logged to every channel the user was in).
    pub fn log_quit(&self, channel: &str, nick: &str, reason: &str) {
        self.inner.log_quit(channel, nick, reason);
    }

    /// Convenience: log a KICK.
    pub fn log_kick(&self, channel: &str, nick: &str, content: &str) {
        self.inner.log_kick(channel, nick, content);
    }

    /// Convenience: log a TOPIC change.
    pub fn log_topic(&self, channel: &str, nick: &str, new_topic: &str) {
        self.inner.log_topic(channel, nick, new_topic);
    }

    /// Convenience: log a NICK change (logged to every shared channel).
    pub fn log_nick_change(&self, channel: &str, old_nick: &str, new_nick: &str) {
        self.inner.log_nick_change(channel, old_nick, new_nick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airc_shared::log::{sanitize_filename, CSV_HEADER};
    use std::fs;

    #[test]
    fn sanitize_channel_name() {
        assert_eq!(sanitize_filename("#lobby"), "lobby");
        assert_eq!(sanitize_filename("&local"), "local");
        assert_eq!(sanitize_filename("alice"), "alice");
        assert_eq!(sanitize_filename("#My/Channel"), "my_channel");
    }

    #[test]
    fn disabled_logger_is_noop() {
        let logger = ChannelLogger::new(None);
        // Should not panic.
        logger.log_message("#test", "nick", "hello");
    }

    #[test]
    fn writes_csv_file() {
        let dir = std::env::temp_dir().join("airc_test_logger");
        let _ = fs::remove_dir_all(&dir);

        let logger = ChannelLogger::new(Some(dir.clone()));
        logger.log_join("#test", "alice");
        logger.log_message("#test", "alice", "hello world");
        logger.log_part("#test", "alice", "bye");

        // Force flush by dropping.
        drop(logger);

        let content = fs::read_to_string(dir.join("test.csv")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines.len(), 4); // header + 3 events
        assert!(lines[1].contains(",join,"));
        assert!(lines[2].contains(",message,"));
        assert!(lines[3].contains(",part,"));

        let _ = fs::remove_dir_all(&dir);
    }
}
