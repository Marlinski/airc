//! AIRC MCP Server library — expose IRC daemon commands as MCP tools.
//!
//! This module runs an MCP server over stdio (JSON-RPC). Each tool
//! corresponds to a CLI command (`airc connect`, `airc join`, etc.) and
//! communicates with the running `airc` daemon via the same Unix socket
//! IPC protocol used by the CLI.
//!
//! # Tools
//!
//! | Tool           | Description                                      |
//! |----------------|--------------------------------------------------|
//! | `connect`      | Start the daemon and connect to an IRC server    |
//! | `disconnect`   | Disconnect and stop the daemon                   |
//! | `join`         | Join an IRC channel                              |
//! | `part`         | Leave an IRC channel                             |
//! | `say`          | Send a message to a channel or user              |
//! | `fetch`        | Fetch new (unread) messages                      |
//! | `status`       | Show connection status and channel info           |
//! | `logs`         | Show recent log events from the daemon's buffer  |
//!
//! CSV logging is enabled automatically on `connect`.

use std::path::PathBuf;

use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use airc_shared::ipc::*;

use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, tool};

// ---------------------------------------------------------------------------
// IPC helpers (duplicated from crates/airc/src/ipc.rs to avoid a lib dep)
// ---------------------------------------------------------------------------

/// Glob pattern for airc session sockets.
const SOCK_GLOB: &str = ".airc-*.sock";

/// Find the session socket in the current directory.
///
/// Expects exactly one `.airc-*.sock` file. Errors if zero or multiple found.
fn discover_socket() -> Result<PathBuf, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("cannot determine working directory: {e}"))?;
    let pattern = cwd.join(SOCK_GLOB);
    let pattern_str = pattern.to_string_lossy();

    let mut sockets = Vec::new();
    if let Ok(entries) = glob::glob(&pattern_str) {
        for entry in entries.flatten() {
            sockets.push(entry);
        }
    }

    match sockets.len() {
        0 => Err("no session found — use the `connect` tool first".to_string()),
        1 => Ok(sockets.into_iter().next().unwrap()),
        n => {
            let names: Vec<_> = sockets
                .iter()
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .collect();
            Err(format!(
                "multiple sessions found ({n}): {}\nhint: disconnect stale sessions first",
                names.join(", ")
            ))
        }
    }
}

/// Write a length-prefixed protobuf frame.
async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    msg: &impl Message,
) -> Result<(), String> {
    let buf = msg.encode_to_vec();
    let len = buf.len() as u32;
    writer
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| format!("write length: {e}"))?;
    writer
        .write_all(&buf)
        .await
        .map_err(|e| format!("write payload: {e}"))?;
    Ok(())
}

/// Read a length-prefixed protobuf frame.
async fn read_frame<R: AsyncReadExt + Unpin, M: Message + Default>(
    reader: &mut R,
) -> Result<M, String> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read length: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(format!("frame too large: {len} bytes"));
    }
    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read payload: {e}"))?;
    M::decode(&payload[..]).map_err(|e| format!("decode: {e}"))
}

/// Send an IPC request to the daemon and return the response.
async fn send_request(req: &IpcRequest) -> Result<IpcResponse, String> {
    let path = discover_socket()?;
    let mut stream = UnixStream::connect(&path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "daemon is not running — use the `connect` tool first".to_string()
            }
            std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
            std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
            _ => e.to_string(),
        })?;

    write_frame(&mut stream, req).await?;
    stream
        .shutdown()
        .await
        .map_err(|e| format!("shutdown write: {e}"))?;
    read_frame::<_, IpcResponse>(&mut stream).await
}

/// Helper: build an IpcRequest, send it, return a formatted CallToolResult.
async fn ipc_call(command: ipc_request::Command) -> Result<CallToolResult, String> {
    let req = IpcRequest {
        command: Some(command),
    };
    let resp = send_request(&req).await?;
    if !resp.ok {
        let err = resp.error.unwrap_or_else(|| "unknown error".to_string());
        return Ok(CallToolResult::error(vec![Content::text(err)]));
    }
    Ok(format_response(resp))
}

/// Convert a successful IpcResponse into a CallToolResult with text content.
fn format_response(resp: IpcResponse) -> CallToolResult {
    match resp.payload {
        Some(ipc_response::Payload::Text(t)) => {
            CallToolResult::success(vec![Content::text(t.text)])
        }

        Some(ipc_response::Payload::Messages(fetch)) => {
            if fetch.messages.is_empty() {
                CallToolResult::success(vec![Content::text("(no new messages)")])
            } else {
                let text = fetch
                    .messages
                    .iter()
                    .map(|msg| {
                        let kind = airc_shared::common::MessageKind::try_from(msg.kind)
                            .unwrap_or(airc_shared::common::MessageKind::Normal);
                        match kind {
                            airc_shared::common::MessageKind::Action => {
                                format!(
                                    "[{}] {}: * {} {}",
                                    msg.timestamp, msg.target, msg.from, msg.text
                                )
                            }
                            airc_shared::common::MessageKind::Normal => {
                                format!(
                                    "[{}] {}: <{}> {}",
                                    msg.timestamp, msg.target, msg.from, msg.text
                                )
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                CallToolResult::success(vec![Content::text(text)])
            }
        }

        Some(ipc_response::Payload::Channels(status)) => {
            let mut lines = vec![format!("connected as {}", status.nick)];
            if status.channels.is_empty() {
                lines.push("(not in any channels)".to_string());
            } else {
                for ch in &status.channels {
                    let topic = ch.topic.as_deref().unwrap_or("(no topic)");
                    lines.push(format!(
                        "  {} — {} members, {} unread — {}",
                        ch.name, ch.members, ch.unread, topic
                    ));
                }
            }
            CallToolResult::success(vec![Content::text(lines.join("\n"))])
        }

        Some(ipc_response::Payload::Logs(logs)) => {
            if logs.events.is_empty() {
                CallToolResult::success(vec![Content::text("(no log events)")])
            } else {
                let text = logs
                    .events
                    .iter()
                    .map(|ev| {
                        let etype = airc_shared::log::event_type_to_str(ev.event_type);
                        format!(
                            "[{}] {} {} <{}> {}",
                            ev.timestamp, ev.channel, etype, ev.nick, ev.content
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                CallToolResult::success(vec![Content::text(text)])
            }
        }

        None => CallToolResult::success(vec![Content::text("ok")]),
    }
}

// ---------------------------------------------------------------------------
// Parameter structs for aggregate tool params
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ConnectParams {
    /// IRC server address (host:port), e.g. "irc.libera.chat:6667".
    server: String,

    /// Nickname to use on the server.
    #[serde(default = "default_nick")]
    nick: String,

    /// Comma-separated list of channels to auto-join after connecting.
    #[serde(default)]
    channels: Option<String>,
}

fn default_nick() -> String {
    "agent".to_string()
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct JoinParams {
    /// Channel name to join, e.g. "#lobby".
    channel: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PartParams {
    /// Channel name to leave.
    channel: String,

    /// Optional reason for leaving.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SayParams {
    /// Target channel or nick to send the message to.
    target: String,

    /// Message text to send.
    message: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FetchParams {
    /// Channel to fetch from (omit for all channels).
    #[serde(default)]
    channel: Option<String>,

    /// Fetch last N messages instead of only unread.
    #[serde(default)]
    last: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct LogsParams {
    /// Number of recent events to return (default: 50).
    #[serde(default = "default_logs_last")]
    last: Option<u32>,

    /// Filter by channel name.
    #[serde(default)]
    channel: Option<String>,
}

fn default_logs_last() -> Option<u32> {
    Some(50)
}

// ---------------------------------------------------------------------------
// MCP Server
// ---------------------------------------------------------------------------

/// The AIRC MCP server. Each tool sends an IPC request to the running
/// `airc` daemon over a Unix socket and returns the formatted result.
#[derive(Debug, Clone, Default)]
struct AircMcpServer;

#[tool(tool_box)]
impl AircMcpServer {
    /// Start the airc daemon and connect to an IRC server.
    ///
    /// This spawns the daemon process which maintains a persistent IRC
    /// connection. The daemon keeps running after this tool returns — use
    /// `disconnect` to stop it.
    #[tool(
        name = "connect",
        description = "Start the airc daemon and connect to an IRC server. Spawns a background daemon that maintains a persistent IRC connection."
    )]
    async fn connect(
        &self,
        #[tool(aggr)] params: ConnectParams,
    ) -> Result<CallToolResult, rmcp::Error> {
        // Find the `airc` binary next to ourselves.
        let exe = find_airc_binary().map_err(|e| {
            rmcp::Error::internal_error(format!("cannot find airc binary: {e}"), None)
        })?;

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("connect")
            .arg(&params.server)
            .arg("--nick")
            .arg(&params.nick);

        if let Some(ref channels) = params.channels {
            cmd.arg("--join").arg(channels);
        }

        // Detach stdio so the daemon runs in the background.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = cmd
            .output()
            .map_err(|e| rmcp::Error::internal_error(format!("failed to spawn airc: {e}"), None))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            let msg = if stdout.is_empty() {
                format!("connecting to {} as {}", params.server, params.nick)
            } else {
                stdout.trim().to_string()
            };

            // Auto-start CSV logging. Give the daemon a moment to bind
            // its socket, then fire-and-forget the log_start command.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = ipc_call(ipc_request::Command::LogStart(LogStartRequest {
                dir: None,
            }))
            .await;

            Ok(CallToolResult::success(vec![Content::text(msg)]))
        } else {
            let err = if stderr.is_empty() {
                stdout.trim().to_string()
            } else {
                stderr.trim().to_string()
            };
            Ok(CallToolResult::error(vec![Content::text(err)]))
        }
    }

    /// Disconnect from the IRC server and stop the daemon.
    #[tool(
        name = "disconnect",
        description = "Disconnect from the IRC server and stop the airc daemon."
    )]
    async fn disconnect(&self) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Disconnect(DisconnectRequest {})).await {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// Join an IRC channel.
    #[tool(
        name = "join",
        description = "Join an IRC channel. The daemon must be running (use `connect` first)."
    )]
    async fn join(&self, #[tool(aggr)] params: JoinParams) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Join(JoinRequest {
            channel: params.channel,
        }))
        .await
        {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// Leave an IRC channel.
    #[tool(
        name = "part",
        description = "Leave (part) an IRC channel, optionally with a reason."
    )]
    async fn part(&self, #[tool(aggr)] params: PartParams) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Part(PartRequest {
            channel: params.channel,
            reason: params.reason,
        }))
        .await
        {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// Send a message to a channel or user.
    #[tool(
        name = "say",
        description = "Send a message to an IRC channel or user (PRIVMSG)."
    )]
    async fn say(&self, #[tool(aggr)] params: SayParams) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Say(SayRequest {
            target: params.target,
            message: params.message,
        }))
        .await
        {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// Fetch new (unread) messages from the IRC connection.
    ///
    /// Returns messages from all channels or a specific channel. By default
    /// returns only unread messages; use `last` to get the N most recent
    /// messages regardless of read cursor.
    #[tool(
        name = "fetch",
        description = "Fetch new (unread) IRC messages. Returns messages from all channels, or a specific channel if specified. Use `last` to get the N most recent messages."
    )]
    async fn fetch(
        &self,
        #[tool(aggr)] params: FetchParams,
    ) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Fetch(FetchRequest {
            channel: params.channel,
            last: params.last,
        }))
        .await
        {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// Show connection status and channel info.
    #[tool(
        name = "status",
        description = "Show the current IRC connection status: connected nick, joined channels, member counts, and unread message counts."
    )]
    async fn status(&self) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Status(StatusRequest {})).await {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    /// Show recent log events from the daemon's in-memory ring buffer.
    #[tool(
        name = "logs",
        description = "Show recent IRC log events from the daemon's in-memory buffer. Returns the last N events, optionally filtered by channel."
    )]
    async fn logs(&self, #[tool(aggr)] params: LogsParams) -> Result<CallToolResult, rmcp::Error> {
        match ipc_call(ipc_request::Command::Logs(LogsRequest {
            last: params.last,
            channel: params.channel,
        }))
        .await
        {
            Ok(r) => Ok(r),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

#[tool(tool_box)]
impl ServerHandler for AircMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "AIRC MCP Server — connect to IRC servers, join channels, send and receive \
                 messages. CSV logging starts automatically on connect. Start with `connect` \
                 to establish an IRC connection."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: rmcp::model::Implementation {
                name: "airc-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the `airc` binary. Since airc-mcp is now part of the `airc` binary
/// itself, `current_exe()` gives us the right path.
fn find_airc_binary() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot determine own path: {e}"))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the MCP server on stdio.
///
/// This function blocks until the MCP client disconnects. Tracing output
/// is sent to stderr so it doesn't interfere with the JSON-RPC transport
/// on stdout.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("starting airc-mcp server");

    let server = AircMcpServer;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;

    Ok(())
}
