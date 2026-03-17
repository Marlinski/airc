//! IPC socket listener for `aircd stop` / `aircd status` commands.
//!
//! The server binds a Unix domain socket at `aircd.sock` (same directory as
//! the PID file). The `aircd` CLI connects to this socket to send shutdown
//! or stats requests using length-prefixed protobuf frames.
//!
//! Wire format: `[4 bytes big-endian length][protobuf payload]` — same as
//! the airc CLI<->daemon IPC.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info, warn};

use airc_shared::aircd_ipc::aircd_request::Command;
use airc_shared::aircd_ipc::*;

use crate::state::SharedState;

/// Signal sent from the IPC handler to the main server loop.
#[derive(Debug)]
pub enum IpcSignal {
    /// Graceful shutdown requested via `aircd stop`.
    Shutdown { reason: String },
}

/// Path to the IPC Unix socket.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("aircd.sock")
}

/// Start the IPC listener. Returns a receiver that yields [`IpcSignal`]s.
///
/// The listener runs in a background task. When a shutdown request is
/// received, the handler sends the response to the client, then forwards
/// the signal to the main server loop via the channel.
pub fn start_listener(state: SharedState) -> Result<(mpsc::Receiver<IpcSignal>, PathBuf), String> {
    let sock_path = socket_path();

    // Remove stale socket if it exists.
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }

    let listener = UnixListener::bind(&sock_path)
        .map_err(|e| format!("cannot bind IPC socket at {}: {e}", sock_path.display()))?;

    info!(path = %sock_path.display(), "IPC listener started");

    let (tx, rx) = mpsc::channel::<IpcSignal>(4);
    let path_clone = sock_path.clone();

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = state.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_ipc_connection(stream, &state, &tx).await {
                            warn!(error = %e, "IPC connection error");
                        }
                    });
                }
                Err(e) => {
                    error!(error = %e, "IPC accept error");
                    break;
                }
            }
        }
    });

    Ok((rx, path_clone))
}

/// Timeout for reading a request frame from an IPC client.
/// Stalled IPC clients would otherwise hold a handler task open indefinitely.
const IPC_READ_TIMEOUT_SECS: u64 = 5;

/// Handle a single IPC connection.
async fn handle_ipc_connection(
    mut stream: UnixStream,
    state: &SharedState,
    shutdown_tx: &mpsc::Sender<IpcSignal>,
) -> Result<(), String> {
    let req: AircdRequest = time::timeout(
        Duration::from_secs(IPC_READ_TIMEOUT_SECS),
        read_frame(&mut stream),
    )
    .await
    .map_err(|_| "IPC read timeout".to_string())??;

    let Some(command) = req.command else {
        let resp = aircd_response_err("empty request (no command)");
        write_frame(&mut stream, &resp).await?;
        return Ok(());
    };

    match command {
        Command::Shutdown(r) => {
            let reason = r.reason.unwrap_or_else(|| "IPC shutdown".to_string());
            info!(reason = %reason, "shutdown requested via IPC");

            // Send response before triggering shutdown.
            let resp = AircdResponse {
                ok: true,
                error: None,
                payload: Some(aircd_response::Payload::Shutdown(ShutdownResponse {
                    message: "shutting down gracefully".to_string(),
                })),
            };
            write_frame(&mut stream, &resp).await?;

            // Signal the main loop.
            let _ = shutdown_tx
                .send(IpcSignal::Shutdown {
                    reason: reason.clone(),
                })
                .await;

            debug!("shutdown signal sent to main loop");
        }

        Command::Stats(_) => {
            let stats = state.stats().await;
            let resp = AircdResponse {
                ok: true,
                error: None,
                payload: Some(aircd_response::Payload::Stats(stats)),
            };
            write_frame(&mut stream, &resp).await?;
            debug!("stats response sent");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Length-prefixed framing (same wire format as airc CLI IPC)
// ---------------------------------------------------------------------------

async fn write_frame(stream: &mut UnixStream, msg: &impl Message) -> Result<(), String> {
    let buf = msg.encode_to_vec();
    let len = buf.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| format!("write length: {e}"))?;
    stream
        .write_all(&buf)
        .await
        .map_err(|e| format!("write payload: {e}"))?;
    stream.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

async fn read_frame<M: Message + Default>(stream: &mut UnixStream) -> Result<M, String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read length: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;

    // Limit frame size to 64 KiB. IPC messages (shutdown/stats) are tiny;
    // a larger limit would allow a local attacker to force a multi-MiB
    // allocation per connection (MISC-4).
    if len > 65536 {
        return Err(format!("frame too large: {len} bytes"));
    }

    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read payload: {e}"))?;

    M::decode(&payload[..]).map_err(|e| format!("decode: {e}"))
}

fn aircd_response_err(msg: &str) -> AircdResponse {
    AircdResponse {
        ok: false,
        error: Some(msg.to_string()),
        payload: None,
    }
}

/// Clean up the IPC socket file.
pub fn cleanup(sock_path: &PathBuf) {
    let _ = fs::remove_file(sock_path);
}
