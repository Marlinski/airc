//! IPC protocol between the CLI commands and the daemon.
//!
//! Communication is over a Unix domain socket using length-prefixed protobuf
//! frames: `[4 bytes big-endian length][protobuf payload]`.
//!
//! Each connection handles exactly one request/response pair.
//!
//! ## Socket convention
//!
//! The daemon creates a single socket file in the working directory:
//!
//!     .airc-<session_id>-<pid>.sock
//!
//! - **session_id**: 8-char random alphanumeric, generated at connect time.
//! - **pid**: the daemon's process ID, useful for diagnostics / kill.
//!
//! Commands discover the socket by globbing for `.airc-*.sock` in the
//! current directory. No separate session or PID file is needed.

use std::path::{Path, PathBuf};

use prost::Message;
use rand::Rng;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// Re-export the proto IPC types so the rest of the crate uses them directly.
pub use airc_shared::ipc::*;

// ---------------------------------------------------------------------------
// Socket naming
// ---------------------------------------------------------------------------

/// Glob pattern that matches any airc session socket in a directory.
const SOCK_GLOB: &str = ".airc-*.sock";

/// Build the socket filename for a given session ID and PID.
pub fn socket_name(session_id: &str, pid: u32) -> String {
    format!(".airc-{session_id}-{pid}.sock")
}

/// Build the full path for a socket in the given directory.
pub fn socket_path(dir: &Path, session_id: &str, pid: u32) -> PathBuf {
    dir.join(socket_name(session_id, pid))
}

/// Generate a short random alphanumeric session ID (8 chars).
pub fn generate_session_id() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Information extracted from a socket filename.
#[derive(Debug, Clone)]
pub struct SocketInfo {
    pub path: PathBuf,
    pub session_id: String,
    /// Daemon PID encoded in the socket filename (for diagnostics / kill).
    #[allow(dead_code)]
    pub pid: u32,
}

/// Parse a socket filename like `.airc-k7m2x9ab-12345.sock` into its parts.
fn parse_socket_name(path: &Path) -> Option<SocketInfo> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix(".airc-")?.strip_suffix(".sock")?;
    let dash = stem.rfind('-')?;
    let session_id = &stem[..dash];
    let pid_str = &stem[dash + 1..];
    let pid = pid_str.parse::<u32>().ok()?;
    Some(SocketInfo {
        path: path.to_path_buf(),
        session_id: session_id.to_string(),
        pid,
    })
}

/// Find all `.airc-*.sock` files in the given directory.
fn find_sockets(dir: &Path) -> Vec<SocketInfo> {
    let pattern = dir.join(SOCK_GLOB);
    let pattern_str = pattern.to_string_lossy();
    let mut results = Vec::new();
    if let Ok(entries) = glob::glob(&pattern_str) {
        for entry in entries.flatten() {
            if let Some(info) = parse_socket_name(&entry) {
                results.push(info);
            }
        }
    }
    results
}

/// Discover the active session socket in the given directory.
///
/// If `session_id` is provided, look for that specific session. Otherwise
/// expect exactly one socket — error if zero or more than one.
pub fn discover_socket(dir: &Path, session_id: Option<&str>) -> Result<SocketInfo, String> {
    let sockets = find_sockets(dir);

    if let Some(id) = session_id {
        // Find the socket matching this session ID.
        let matches: Vec<_> = sockets.into_iter().filter(|s| s.session_id == id).collect();
        match matches.len() {
            0 => Err(format!(
                "no socket found for session {id} — is the daemon running?"
            )),
            1 => Ok(matches.into_iter().next().unwrap()),
            n => Err(format!(
                "multiple sockets ({n}) for session {id} — this shouldn't happen"
            )),
        }
    } else {
        match sockets.len() {
            0 => Err(
                "no session found — run `airc connect` first, or use `--session <id>`".to_string(),
            ),
            1 => Ok(sockets.into_iter().next().unwrap()),
            n => {
                let ids: Vec<_> = sockets.iter().map(|s| s.session_id.as_str()).collect();
                Err(format!(
                    "multiple sessions found ({n}): {}\n\
                     hint: use `--session <id>` to pick one",
                    ids.join(", ")
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Length-prefixed framing helpers
// ---------------------------------------------------------------------------

/// Write a length-prefixed protobuf message to a writer.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &impl Message,
) -> Result<(), String> {
    let buf = msg.encode_to_vec();
    let len = buf.len() as u32;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| format!("write length error: {e}"))?;
    writer
        .write_all(&buf)
        .await
        .map_err(|e| format!("write payload error: {e}"))?;
    Ok(())
}

/// Read a length-prefixed protobuf message from a reader.
pub async fn read_frame<R: AsyncReadExt + Unpin, M: Message + Default>(
    reader: &mut R,
) -> Result<M, String> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read length error: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;

    // Sanity check: reject absurdly large frames (> 16 MB).
    if len > 16 * 1024 * 1024 {
        return Err(format!("frame too large: {len} bytes"));
    }

    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read payload error: {e}"))?;

    M::decode(&payload[..]).map_err(|e| format!("decode error: {e}"))
}

// ---------------------------------------------------------------------------
// Convenience builders for IpcResponse
// ---------------------------------------------------------------------------

/// Quick success with text.
pub fn response_ok(text: &str) -> IpcResponse {
    IpcResponse {
        ok: true,
        error: None,
        payload: Some(ipc_response::Payload::Text(TextPayload {
            text: text.to_string(),
        })),
    }
}

/// Quick error.
pub fn response_err(msg: &str) -> IpcResponse {
    IpcResponse {
        ok: false,
        error: Some(msg.to_string()),
        payload: None,
    }
}

/// Response with fetched messages.
pub fn response_messages(messages: Vec<airc_shared::common::ChannelMessage>) -> IpcResponse {
    IpcResponse {
        ok: true,
        error: None,
        payload: Some(ipc_response::Payload::Messages(FetchPayload { messages })),
    }
}

/// Response with channel status.
pub fn response_status(
    nick: String,
    channels: Vec<airc_shared::common::ChannelStatus>,
) -> IpcResponse {
    IpcResponse {
        ok: true,
        error: None,
        payload: Some(ipc_response::Payload::Channels(StatusPayload {
            nick,
            channels,
        })),
    }
}

/// Response with log events.
pub fn response_logs(events: Vec<airc_shared::common::LogEvent>) -> IpcResponse {
    IpcResponse {
        ok: true,
        error: None,
        payload: Some(ipc_response::Payload::Logs(LogsPayload { events })),
    }
}

// ---------------------------------------------------------------------------
// Client side (CLI commands -> daemon)
// ---------------------------------------------------------------------------

/// Send a request to the daemon and return the response.
///
/// The caller is responsible for discovering the socket path first
/// (via [`discover_socket`]).
pub async fn send_request(sock_path: &Path, req: &IpcRequest) -> Result<IpcResponse, String> {
    let mut stream = UnixStream::connect(sock_path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "socket not found — daemon is not running".to_string(),
            std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            _ => e.to_string(),
        })?;

    // Send request as a length-prefixed protobuf frame.
    write_frame(&mut stream, req).await?;

    // Shut down the write half so the daemon knows the request is complete.
    stream
        .shutdown()
        .await
        .map_err(|e| format!("shutdown write error: {e}"))?;

    // Read response.
    read_frame::<_, IpcResponse>(&mut stream).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -- parse_socket_name --------------------------------------------------

    #[test]
    fn parse_valid_socket_name() {
        let path = PathBuf::from("/tmp/.airc-k7m2x9ab-12345.sock");
        let info = parse_socket_name(&path).expect("should parse");
        assert_eq!(info.session_id, "k7m2x9ab");
        assert_eq!(info.pid, 12345);
        assert_eq!(info.path, path);
    }

    #[test]
    fn parse_socket_name_with_large_pid() {
        let path = PathBuf::from(".airc-abc12345-4294967295.sock");
        let info = parse_socket_name(&path).expect("should parse max u32 pid");
        assert_eq!(info.session_id, "abc12345");
        assert_eq!(info.pid, u32::MAX);
    }

    #[test]
    fn parse_socket_name_missing_prefix() {
        let path = PathBuf::from("airc-abcd1234-100.sock");
        assert!(parse_socket_name(&path).is_none());
    }

    #[test]
    fn parse_socket_name_missing_suffix() {
        let path = PathBuf::from(".airc-abcd1234-100.socket");
        assert!(parse_socket_name(&path).is_none());
    }

    #[test]
    fn parse_socket_name_no_dash_between_id_and_pid() {
        let path = PathBuf::from(".airc-abcd1234.sock");
        // No dash → rfind('-') finds the one after ".airc-" prefix strip,
        // but there is none in "abcd1234", so parse fails.
        assert!(parse_socket_name(&path).is_none());
    }

    #[test]
    fn parse_socket_name_pid_not_numeric() {
        let path = PathBuf::from(".airc-abcd1234-notapid.sock");
        assert!(parse_socket_name(&path).is_none());
    }

    #[test]
    fn parse_socket_name_pid_overflow() {
        // u32::MAX + 1 = 4294967296, doesn't fit in u32
        let path = PathBuf::from(".airc-abcd1234-4294967296.sock");
        assert!(parse_socket_name(&path).is_none());
    }

    #[test]
    fn parse_socket_name_empty_session_id() {
        // Session ID is empty: ".airc--100.sock"
        // After stripping prefix/suffix we get "-100", rfind('-') at 0,
        // session_id = "" (empty), pid = 100. This parses but session_id is empty.
        let path = PathBuf::from(".airc--100.sock");
        let info = parse_socket_name(&path).expect("parses syntactically");
        assert_eq!(info.session_id, "");
        assert_eq!(info.pid, 100);
    }

    // -- socket_name / socket_path ------------------------------------------

    #[test]
    fn socket_name_format() {
        assert_eq!(socket_name("abc12345", 42), ".airc-abc12345-42.sock");
    }

    #[test]
    fn socket_path_joins_dir() {
        let p = socket_path(Path::new("/home/user/project"), "sess0001", 99);
        assert_eq!(
            p,
            PathBuf::from("/home/user/project/.airc-sess0001-99.sock")
        );
    }

    // -- generate_session_id ------------------------------------------------

    #[test]
    fn session_id_length_and_charset() {
        for _ in 0..20 {
            let id = generate_session_id();
            assert_eq!(id.len(), 8);
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            );
        }
    }

    // -- find_sockets / discover_socket -------------------------------------

    /// Helper: create a fake socket file (regular file, not an actual socket).
    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), b"").unwrap();
    }

    #[test]
    fn find_sockets_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sockets = find_sockets(dir.path());
        assert!(sockets.is_empty());
    }

    #[test]
    fn find_sockets_one_socket() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-abcd1234-100.sock");
        let sockets = find_sockets(dir.path());
        assert_eq!(sockets.len(), 1);
        assert_eq!(sockets[0].session_id, "abcd1234");
        assert_eq!(sockets[0].pid, 100);
    }

    #[test]
    fn find_sockets_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-abcd1234-100.sock");
        touch(dir.path(), "README.md");
        touch(dir.path(), ".airc-notes.txt");
        touch(dir.path(), "airc-wrong-200.sock"); // missing dot prefix
        let sockets = find_sockets(dir.path());
        assert_eq!(sockets.len(), 1);
    }

    #[test]
    fn find_sockets_multiple() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-aaaaaaaa-1.sock");
        touch(dir.path(), ".airc-bbbbbbbb-2.sock");
        let sockets = find_sockets(dir.path());
        assert_eq!(sockets.len(), 2);
    }

    #[test]
    fn discover_socket_single() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-abcd1234-100.sock");
        let info = discover_socket(dir.path(), None).expect("should find one");
        assert_eq!(info.session_id, "abcd1234");
    }

    #[test]
    fn discover_socket_none_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = discover_socket(dir.path(), None).unwrap_err();
        assert!(err.contains("no session found"));
    }

    #[test]
    fn discover_socket_multiple_without_filter() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-aaaaaaaa-1.sock");
        touch(dir.path(), ".airc-bbbbbbbb-2.sock");
        let err = discover_socket(dir.path(), None).unwrap_err();
        assert!(err.contains("multiple sessions"));
        assert!(err.contains("--session"));
    }

    #[test]
    fn discover_socket_with_session_filter() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-aaaaaaaa-1.sock");
        touch(dir.path(), ".airc-bbbbbbbb-2.sock");
        let info = discover_socket(dir.path(), Some("bbbbbbbb")).expect("should find matching");
        assert_eq!(info.session_id, "bbbbbbbb");
        assert_eq!(info.pid, 2);
    }

    #[test]
    fn discover_socket_with_session_filter_not_found() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".airc-aaaaaaaa-1.sock");
        let err = discover_socket(dir.path(), Some("zzzzzzzz")).unwrap_err();
        assert!(err.contains("no socket found for session zzzzzzzz"));
    }
}
