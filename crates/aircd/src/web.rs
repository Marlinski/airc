//! HTTP API server — REST API, Prometheus metrics, and WebSocket IRC transport.
//!
//! Endpoints:
//! - `GET /api/stats`             — server statistics (users, channels, uptime)
//! - `GET /api/channels`          — list of channels with details
//! - `GET /metrics`               — Prometheus exposition format
//! - `GET /ws`                    — WebSocket upgrade for IRC-over-WebSocket

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::connection::Connection;
use crate::state::SharedState;

// Re-export the proto HTTP API types so `state.rs` can reference them
// as `web::StatsResponse` etc. without needing a separate import.
pub use airc_shared::http_api::{ChannelInfo, ChannelsResponse, StatsResponse};

// ---------------------------------------------------------------------------
// REST API handlers
// ---------------------------------------------------------------------------

async fn get_stats(State(state): State<Arc<SharedState>>) -> Json<StatsResponse> {
    let stats = state.api_stats().await;
    Json(stats)
}

async fn get_channels(State(state): State<Arc<SharedState>>) -> Json<ChannelsResponse> {
    let channels = state.api_channels().await;
    Json(ChannelsResponse { channels })
}

/// Prometheus exposition format metrics.
async fn get_metrics(State(state): State<Arc<SharedState>>) -> impl IntoResponse {
    let stats = state.prometheus_stats().await;

    let mut buf = String::with_capacity(512);

    buf.push_str("# HELP aircd_users_online Number of connected IRC clients.\n");
    buf.push_str("# TYPE aircd_users_online gauge\n");
    buf.push_str(&format!("aircd_users_online {}\n", stats.users_online));

    buf.push_str("# HELP aircd_channels_active Number of active channels.\n");
    buf.push_str("# TYPE aircd_channels_active gauge\n");
    buf.push_str(&format!(
        "aircd_channels_active {}\n",
        stats.channels_active
    ));

    buf.push_str("# HELP aircd_uptime_seconds Server uptime in seconds.\n");
    buf.push_str("# TYPE aircd_uptime_seconds counter\n");
    buf.push_str(&format!("aircd_uptime_seconds {}\n", stats.uptime_seconds));

    for (name, count) in &stats.channel_counts {
        buf.push_str(&format!(
            "aircd_channel_members{{channel=\"{name}\"}} {count}\n"
        ));
    }

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        buf,
    )
}

// ---------------------------------------------------------------------------
// WebSocket IRC transport
// ---------------------------------------------------------------------------

/// Handle the WebSocket upgrade request.
async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<SharedState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state, addr))
}

/// Handle a single WebSocket connection as an IRC client.
///
/// IRC lines flow as WebSocket **text** frames (one IRC message per frame,
/// no `\r\n` framing needed — though we tolerate trailing `\r\n`).
///
/// Internally this bridges the WebSocket into the same `Connection` lifecycle
/// used by TCP clients:
/// - Incoming WS text frames → written into a pipe → `BufReader` → `Connection::run_generic()`
/// - Outgoing IRC lines from `mpsc::Sender<String>` → sent as WS text frames
///
/// # Ping / pong idle timeout
///
/// A WS `Ping` is sent every 60 seconds via a dedicated control channel.
/// `last_pong` is updated whenever a `Pong` frame arrives in the reader task.
/// If no `Pong` is received within 90 seconds of the last ping (60 s interval
/// + 30 s grace period), the reader task drops the pipe writer (signalling EOF
/// to the `Connection`) and the writer task sends a `Close` frame.
///
/// Constants:
/// - `PING_INTERVAL` — how often to send a WS Ping (60 s)
/// - `PONG_TIMEOUT`  — max age of `last_pong` before we close (90 s)

/// Control messages sent from the reader task to the writer task.
enum WsCtrl {
    /// Ask the writer to send a WS Ping frame.
    SendPing,
    /// Ask the writer to close the WS connection gracefully.
    Close,
}

async fn handle_ws_connection(
    socket: WebSocket,
    state: Arc<SharedState>,
    addr: std::net::SocketAddr,
) {
    /// How often to send a WS Ping frame.
    const PING_INTERVAL: Duration = Duration::from_secs(60);
    /// Maximum elapsed time since the last Pong before we close the connection.
    const PONG_TIMEOUT: Duration = Duration::from_secs(90);

    let id = state.next_client_id();
    let hostname = addr.ip().to_string();
    info!(client_id = %id, peer = %addr, "new WebSocket connection");

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Channel for outgoing IRC lines (Connection → WebSocket).
    let (tx, mut rx) = mpsc::channel::<Arc<str>>(512);
    let cancel = CancellationToken::new();

    // Channel for control messages (reader task → writer task).
    // Capacity 4 is more than enough; we only ever send Ping or Close.
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<WsCtrl>(4);

    // Pipe for incoming IRC lines (WebSocket → Connection).
    // We write incoming WS text frames (with \n appended) into `pipe_writer`;
    // the Connection reads from `pipe_reader` via BufReader.
    let (pipe_reader, mut pipe_writer) = tokio::io::duplex(8192);

    // --- Writer task: drain outgoing mpsc and send as WS text frames ---
    // Also handles Ping and Close control messages from the reader task.
    let cancel_writer = cancel.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel_writer.cancelled() => break,
                ctrl = ctrl_rx.recv() => {
                    match ctrl {
                        None | Some(WsCtrl::Close) => {
                            // Send a graceful close frame and stop.
                            let _ = ws_sink.send(Message::Close(None)).await;
                            break;
                        }
                        Some(WsCtrl::SendPing) => {
                            if ws_sink.send(Message::Ping(vec![].into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                maybe_line = rx.recv() => {
                    match maybe_line {
                        None => break,
                        Some(line) => {
                            // Convert Arc<str> → &str → Utf8Bytes directly,
                            // avoiding the intermediate String allocation that
                            // line.to_string().into() would create.
                            if ws_sink
                                .send(Message::Text((&*line).into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                }
            }
        }
        let _ = ws_sink.close().await;
    });

    // --- Reader task: read WS frames and pipe into the Connection's reader ---
    //
    // Ping/pong idle timeout:
    //   - A WS Ping is sent every 60 seconds (via ctrl_tx → writer task).
    //   - `last_pong` is updated whenever a Pong frame arrives.
    //   - If `last_pong` is older than 90 seconds when the next ping tick
    //     fires, we drop the pipe (EOF → Connection) and send a Close via
    //     the control channel.
    let reader_handle = tokio::spawn(async move {
        let mut ping_ticker = interval(PING_INTERVAL);
        // Consume the immediate first tick — we do not want to ping the client
        // the instant the connection is established.
        ping_ticker.tick().await;

        let mut last_pong: Instant = Instant::now();
        // True once we have sent at least one ping and are awaiting a pong.
        let mut waiting_for_pong = false;

        loop {
            tokio::select! {
                biased;

                // Ping ticker fires every 60 seconds.
                _ = ping_ticker.tick() => {
                    // Check for idle timeout first.
                    if waiting_for_pong && last_pong.elapsed() > PONG_TIMEOUT {
                        warn!(
                            client_id = %id,
                            "WebSocket idle timeout — no pong received, closing connection",
                        );
                        // Drop the pipe to signal EOF to the Connection.
                        drop(pipe_writer);
                        // Ask the writer task to send a Close frame.
                        let _ = ctrl_tx.send(WsCtrl::Close).await;
                        return;
                    }
                    // Send a Ping frame through the writer task.
                    waiting_for_pong = true;
                    if ctrl_tx.send(WsCtrl::SendPing).await.is_err() {
                        break;
                    }
                    debug!(client_id = %id, "WebSocket ping sent");
                }

                maybe_msg = ws_stream.next() => {
                    match maybe_msg {
                        None => break,
                        Some(Ok(Message::Text(text))) => {
                            // Write the line + \n so BufReader::read_line works.
                            let mut line = text.to_string();
                            if !line.ends_with('\n') {
                                line.push('\n');
                            }
                            if pipe_writer.write_all(line.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) => break,
                        Some(Ok(Message::Pong(_))) => {
                            last_pong = Instant::now();
                            waiting_for_pong = false;
                            debug!(client_id = %id, "WebSocket pong received");
                        }
                        Some(Ok(Message::Ping(_))) => {
                            // axum handles pong replies automatically.
                        }
                        Some(Ok(Message::Binary(_))) => {
                            debug!(client_id = %id, "ignoring binary WS frame");
                        }
                        Some(Err(e)) => {
                            debug!(client_id = %id, error = %e, "WS read error");
                            break;
                        }
                    }
                }
            }
        }
        // Close the pipe so the Connection sees EOF.
        drop(pipe_writer);
    });

    // --- Run the IRC Connection over the pipe reader + mpsc sender ---
    let conn = Connection::new(id, (*state).clone(), hostname);
    conn.run_generic(BufReader::new(pipe_reader), tx, cancel)
        .await;

    // Connection is done — clean up the WS tasks.
    reader_handle.abort();
    writer_handle.abort();
}

// ---------------------------------------------------------------------------
// Router construction
// ---------------------------------------------------------------------------

/// Build the HTTP router with API routes, metrics, and WebSocket endpoint.
pub fn router(state: SharedState) -> Router {
    let shared = Arc::new(state);

    Router::new()
        .route("/api/stats", get(get_stats))
        .route("/api/channels", get(get_channels))
        .route("/metrics", get(get_metrics))
        .route("/ws", get(ws_upgrade))
        .with_state(shared)
}

/// Start the HTTP server on the given address.
pub async fn serve(addr: &str, state: SharedState) -> std::io::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "HTTP/WebSocket server listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
