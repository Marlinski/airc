//! IPC protocol between the CLI commands and the daemon.
//!
//! Communication is over a Unix domain socket using length-prefixed protobuf
//! frames: `[4 bytes big-endian length][protobuf payload]`.
//!
//! Each connection handles exactly one request/response pair.

use std::path::PathBuf;

use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// Re-export the proto IPC types so the rest of the crate uses them directly.
pub use airc_shared::ipc::*;

// ---------------------------------------------------------------------------
// Socket / PID paths
// ---------------------------------------------------------------------------

/// Path to the daemon's Unix socket.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("airc.sock")
}

/// Path to the daemon's PID file.
pub fn pid_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("airc.pid")
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
pub async fn send_request(req: &IpcRequest) -> Result<IpcResponse, String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).await.map_err(|e| {
        let reason = match e.kind() {
            std::io::ErrorKind::NotFound => "socket not found — daemon is not running".to_string(),
            std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            _ => e.to_string(),
        };
        reason
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
