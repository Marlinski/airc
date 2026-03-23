//! CSV log file support for append-only channel logs.
//!
//! Provides CSV serialization/parsing and [`FileLogger`] on top of the
//! protobuf-generated [`EventType`] and [`LogEvent`] from `common.proto`.
//!
//! CSV column order:
//!
//! ```text
//! seq,node_id,timestamp,event_type,channel,nick,content
//! ```
//!
//! - `seq`        — monotonic u64 counter scoped to this logger instance
//! - `node_id`    — originating node identity string
//! - `timestamp`  — RFC 3339 (e.g. `2026-03-13T14:05:00Z`)
//! - `event_type` — lowercase enum tag
//! - `channel`    — channel name or DM peer nick (empty for server-wide events)
//! - `nick`       — who triggered the event
//! - `content`    — message text, reason, new topic, etc. (may be empty)
//!
//! Fields containing commas, quotes, or newlines are quoted per RFC 4180.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

// Re-export proto types so downstream crates can use `airc_shared::log::{EventType, LogEvent}`.
pub use crate::common::{EventType, LogEvent};

// ---------------------------------------------------------------------------
// EventType helpers
// ---------------------------------------------------------------------------

/// Convert an `EventType` (i32 enum) to the lowercase tag used in CSV files.
pub fn event_type_to_str(et: i32) -> &'static str {
    match EventType::try_from(et) {
        Ok(EventType::Message) => "message",
        Ok(EventType::Join) => "join",
        Ok(EventType::Part) => "part",
        Ok(EventType::Quit) => "quit",
        Ok(EventType::Kick) => "kick",
        Ok(EventType::Topic) => "topic",
        Ok(EventType::Nick) => "nick",
        Ok(EventType::Notice) => "notice",
        Ok(EventType::Away) => "away",
        Ok(EventType::Account) => "account",
        Err(_) => "unknown",
    }
}

/// Parse a lowercase CSV tag into an `EventType` i32 value.
pub fn event_type_from_str(s: &str) -> Option<i32> {
    let et = match s {
        "message" => EventType::Message,
        "join" => EventType::Join,
        "part" => EventType::Part,
        "quit" => EventType::Quit,
        "kick" => EventType::Kick,
        "topic" => EventType::Topic,
        "nick" => EventType::Nick,
        "notice" => EventType::Notice,
        "away" => EventType::Away,
        "account" => EventType::Account,
        _ => return None,
    };
    Some(et as i32)
}

// ---------------------------------------------------------------------------
// LogEvent CSV extensions
// ---------------------------------------------------------------------------

/// The CSV header line (for new files).
pub const CSV_HEADER: &str = "seq,node_id,timestamp,event_type,channel,nick,content";

/// Create a new `LogEvent` with the current UTC timestamp.
/// `seq` and `node_id` are stamped by `FileLoggerInner::write()` — pass 0 / ""
/// here and they will be overwritten before serialization.
pub fn log_event_now(event_type: EventType, channel: &str, nick: &str, content: &str) -> LogEvent {
    LogEvent {
        seq: 0,
        node_id: String::new(),
        timestamp: utc_now_rfc3339(),
        event_type: event_type as i32,
        channel: channel.to_string(),
        nick: nick.to_string(),
        content: content.to_string(),
    }
}

/// Serialize a `LogEvent` to a single CSV line (no trailing newline).
/// `seq` and `node_id` are included as the first two columns.
pub fn log_event_to_csv(event: &LogEvent) -> String {
    format!(
        "{},{},{},{},{},{},{}",
        event.seq,
        csv_field(&event.node_id),
        csv_field(&event.timestamp),
        event_type_to_str(event.event_type),
        csv_field(&event.channel),
        csv_field(&event.nick),
        csv_field(&event.content),
    )
}

/// Parse a CSV line back into a `LogEvent`.
pub fn log_event_from_csv(line: &str) -> Option<LogEvent> {
    let fields = parse_csv_fields(line)?;
    if fields.len() < 7 {
        return None;
    }
    Some(LogEvent {
        seq: fields[0].parse::<u64>().ok()?,
        node_id: fields[1].clone(),
        timestamp: fields[2].clone(),
        event_type: event_type_from_str(&fields[3])?,
        channel: fields[4].clone(),
        nick: fields[5].clone(),
        content: fields[6].clone(),
    })
}

// ---------------------------------------------------------------------------
// FileLogger — reusable append-only CSV writer, one file per channel/PM
// ---------------------------------------------------------------------------

/// Append-only CSV file logger. One file per channel or DM target.
///
/// Thread-safe via internal `Mutex`. Writes are fast (single line append)
/// and contention is low, so a `Mutex` is fine.
///
/// When `log_dir` is `None`, all writes are no-ops.
pub struct FileLogger {
    inner: Option<std::sync::Mutex<FileLoggerInner>>,
}

struct FileLoggerInner {
    log_dir: PathBuf,
    node_id: String,
    seq: u64,
    files: HashMap<String, File>,
}

impl FileLogger {
    /// Create a logger. Pass `None` for `log_dir` to disable logging (all calls become no-ops).
    /// `node_id` identifies the originating node in every log row.
    pub fn new(log_dir: Option<PathBuf>, node_id: impl Into<String>) -> Self {
        let node_id = node_id.into();
        match log_dir {
            Some(dir) => {
                if let Err(_e) = fs::create_dir_all(&dir) {
                    return Self { inner: None };
                }
                Self {
                    inner: Some(std::sync::Mutex::new(FileLoggerInner {
                        log_dir: dir,
                        node_id,
                        seq: 0,
                        files: HashMap::new(),
                    })),
                }
            }
            None => Self { inner: None },
        }
    }

    /// Whether logging is currently active.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    /// Record a log event. No-op if logging is disabled.
    /// `seq` and `node_id` on the event are overwritten by the inner writer.
    pub fn log(&self, event: &LogEvent) {
        let Some(ref mutex) = self.inner else { return };
        let Ok(mut inner) = mutex.lock() else { return };
        inner.write(event);
    }

    /// Convenience: log a message.
    pub fn log_message(&self, channel: &str, nick: &str, text: &str) {
        self.log(&log_event_now(EventType::Message, channel, nick, text));
    }

    /// Convenience: log a NOTICE.
    pub fn log_notice(&self, channel: &str, nick: &str, text: &str) {
        self.log(&log_event_now(EventType::Notice, channel, nick, text));
    }

    /// Convenience: log a JOIN.
    pub fn log_join(&self, channel: &str, nick: &str) {
        self.log(&log_event_now(EventType::Join, channel, nick, ""));
    }

    /// Convenience: log a PART.
    pub fn log_part(&self, channel: &str, nick: &str, reason: &str) {
        self.log(&log_event_now(EventType::Part, channel, nick, reason));
    }

    /// Convenience: log a QUIT. Use `channel = ""` for server-wide events.
    pub fn log_quit(&self, channel: &str, nick: &str, reason: &str) {
        self.log(&log_event_now(EventType::Quit, channel, nick, reason));
    }

    /// Convenience: log a KICK.
    pub fn log_kick(&self, channel: &str, nick: &str, content: &str) {
        self.log(&log_event_now(EventType::Kick, channel, nick, content));
    }

    /// Convenience: log a TOPIC change.
    pub fn log_topic(&self, channel: &str, nick: &str, new_topic: &str) {
        self.log(&log_event_now(EventType::Topic, channel, nick, new_topic));
    }

    /// Convenience: log a NICK change. Use `channel = ""` for server-wide events.
    pub fn log_nick_change(&self, channel: &str, old_nick: &str, new_nick: &str) {
        self.log(&log_event_now(EventType::Nick, channel, old_nick, new_nick));
    }
}

impl FileLoggerInner {
    fn write(&mut self, event: &LogEvent) {
        // Stamp seq and node_id — caller leaves these as defaults.
        let mut event = event.clone();
        event.seq = self.seq;
        event.node_id = self.node_id.clone();
        self.seq += 1;

        let filename = sanitize_filename(&event.channel);
        let line = log_event_to_csv(&event);

        let file = self.files.entry(filename.clone()).or_insert_with(|| {
            let path = self.log_dir.join(format!("{filename}.csv"));
            let is_new = !path.exists();
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|e| {
                    panic!("cannot open log file {}: {e}", path.display());
                });
            if is_new {
                let _ = writeln!(f, "{}", CSV_HEADER);
            }
            f
        });

        let _ = writeln!(file, "{line}");
    }
}

/// Turn a channel name like `#lobby` into a safe filename like `lobby`.
/// Empty channel (server-wide events) maps to `_server`.
pub fn sanitize_filename(name: &str) -> String {
    if name.is_empty() {
        return "_server".to_string();
    }
    name.trim_start_matches(['#', '&'])
        .replace(['/', '\\'], "_")
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// CSV helpers (RFC 4180)
// ---------------------------------------------------------------------------

/// Quote a field if it contains commas, quotes, or newlines.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Parse a CSV line into fields, handling quoted fields per RFC 4180.
fn parse_csv_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut chars = line.chars().peekable();

    loop {
        if chars.peek().is_none() {
            // Trailing comma produces an empty final field.
            if line.ends_with(',') {
                fields.push(String::new());
            }
            break;
        }

        if chars.peek() == Some(&'"') {
            // Quoted field.
            chars.next(); // consume opening quote
            let mut field = String::new();
            loop {
                match chars.next() {
                    Some('"') => {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            field.push('"');
                        } else {
                            break; // end of quoted field
                        }
                    }
                    Some(c) => field.push(c),
                    None => break,
                }
            }
            fields.push(field);
            // Skip comma separator.
            if chars.peek() == Some(&',') {
                chars.next();
            }
        } else {
            // Unquoted field.
            let mut field = String::new();
            loop {
                match chars.peek() {
                    Some(&',') => {
                        chars.next();
                        break;
                    }
                    Some(_) => field.push(chars.next().unwrap()),
                    None => break,
                }
            }
            fields.push(field);
        }
    }

    Some(fields)
}

// ---------------------------------------------------------------------------
// Time helper (no chrono dependency — uses std only)
// ---------------------------------------------------------------------------

/// Returns the current UTC time in RFC 3339 format.
///
/// Uses `std::time::SystemTime` to avoid pulling in `chrono`.
pub fn utc_now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();

    // Break epoch seconds into date/time components.
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    // Convert days since epoch to y/m/d.
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm from Howard Hinnant.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(
        seq: u64,
        node_id: &str,
        et: EventType,
        channel: &str,
        nick: &str,
        content: &str,
    ) -> LogEvent {
        LogEvent {
            seq,
            node_id: node_id.to_string(),
            timestamp: "2026-03-13T14:05:00Z".to_string(),
            event_type: et as i32,
            channel: channel.to_string(),
            nick: nick.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn roundtrip_simple() {
        let event = make_event(
            42,
            "abc123",
            EventType::Message,
            "#lobby",
            "alice",
            "hello world",
        );
        let csv = log_event_to_csv(&event);
        assert_eq!(
            csv,
            "42,abc123,2026-03-13T14:05:00Z,message,#lobby,alice,hello world"
        );
        let parsed = log_event_from_csv(&csv).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn roundtrip_quoted_content() {
        let event = make_event(
            0,
            "node1",
            EventType::Message,
            "#lobby",
            "bob",
            "hello, \"world\"",
        );
        let csv = log_event_to_csv(&event);
        assert!(csv.contains("\"hello, \"\"world\"\"\""));
        let parsed = log_event_from_csv(&csv).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn roundtrip_empty_content() {
        let event = make_event(1, "node1", EventType::Join, "#lobby", "carol", "");
        let csv = log_event_to_csv(&event);
        assert_eq!(csv, "1,node1,2026-03-13T14:05:00Z,join,#lobby,carol,");
        let parsed = log_event_from_csv(&csv).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn roundtrip_server_wide_event() {
        let event = make_event(7, "node2", EventType::Quit, "", "dave", "leaving");
        let csv = log_event_to_csv(&event);
        assert_eq!(csv, "7,node2,2026-03-13T14:05:00Z,quit,,dave,leaving");
        let parsed = log_event_from_csv(&csv).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn all_event_types_roundtrip() {
        let types = [
            EventType::Message,
            EventType::Join,
            EventType::Part,
            EventType::Quit,
            EventType::Kick,
            EventType::Topic,
            EventType::Nick,
            EventType::Notice,
        ];
        for et in types {
            let s = event_type_to_str(et as i32);
            assert_eq!(event_type_from_str(s), Some(et as i32));
        }
    }

    #[test]
    fn parse_unknown_event_type() {
        assert_eq!(event_type_from_str("unknown"), None);
    }

    #[test]
    fn csv_field_no_quoting() {
        assert_eq!(csv_field("hello"), "hello");
    }

    #[test]
    fn csv_field_with_comma() {
        assert_eq!(csv_field("a,b"), "\"a,b\"");
    }

    #[test]
    fn csv_field_with_quotes() {
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn now_produces_valid_timestamp() {
        let event = log_event_now(EventType::Join, "#test", "nick", "");
        // Should be parseable RFC 3339: YYYY-MM-DDTHH:MM:SSZ
        assert!(event.timestamp.ends_with('Z'));
        assert_eq!(event.timestamp.len(), 20);
    }

    #[test]
    fn sanitize_channel_names() {
        assert_eq!(sanitize_filename("#lobby"), "lobby");
        assert_eq!(sanitize_filename("&#weird/name"), "weird_name");
        assert_eq!(sanitize_filename("DM_user"), "dm_user");
        assert_eq!(sanitize_filename(""), "_server");
    }

    #[test]
    fn file_logger_stamps_seq_and_node_id() {
        let dir = tempfile::tempdir().unwrap();
        let logger = FileLogger::new(Some(dir.path().to_path_buf()), "testnode");
        logger.log_join("#test", "alice");
        logger.log_join("#test", "bob");

        let content = std::fs::read_to_string(dir.path().join("test.csv")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // header + 2 data rows
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], CSV_HEADER);

        let row0 = log_event_from_csv(lines[1]).unwrap();
        assert_eq!(row0.seq, 0);
        assert_eq!(row0.node_id, "testnode");

        let row1 = log_event_from_csv(lines[2]).unwrap();
        assert_eq!(row1.seq, 1);
        assert_eq!(row1.node_id, "testnode");
    }

    #[test]
    fn file_logger_server_wide_goes_to_server_file() {
        let dir = tempfile::tempdir().unwrap();
        let logger = FileLogger::new(Some(dir.path().to_path_buf()), "n1");
        logger.log_quit("", "dave", "bye");

        assert!(dir.path().join("_server.csv").exists());
        assert!(!dir.path().join(".csv").exists());
    }
}
