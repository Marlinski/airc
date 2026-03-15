//! HTTP API server — REST API, Prometheus metrics, and WebSocket IRC transport.
//!
//! Endpoints:
//! - `GET /api/stats`             — server statistics (users, channels, uptime)
//! - `GET /api/channels`          — list of channels with details
//! - `GET /metrics`               — Prometheus exposition format
//! - `GET /ws`                    — WebSocket upgrade for IRC-over-WebSocket

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, info};

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
    let stats = state.stats().await;

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

    for ch in &stats.channels {
        let name = &ch.name;
        buf.push_str(&format!(
            "aircd_channel_members{{channel=\"{name}\"}} {}\n",
            ch.member_count
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
async fn handle_ws_connection(
    socket: WebSocket,
    state: Arc<SharedState>,
    addr: std::net::SocketAddr,
) {
    let id = state.next_client_id();
    let hostname = addr.ip().to_string();
    info!(client_id = %id, peer = %addr, "new WebSocket connection");

    let (mut ws_sink, mut ws_stream) = socket.split();

    // Channel for outgoing IRC lines (Connection → WebSocket).
    let (tx, mut rx) = mpsc::channel::<Arc<str>>(512);

    // Pipe for incoming IRC lines (WebSocket → Connection).
    // We write incoming WS text frames (with \n appended) into `pipe_writer`;
    // the Connection reads from `pipe_reader` via BufReader.
    let (pipe_reader, mut pipe_writer) = tokio::io::duplex(8192);

    // --- Writer task: drain outgoing mpsc and send as WS text frames ---
    let writer_handle = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if ws_sink
                .send(Message::Text(line.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = ws_sink.close().await;
    });

    // --- Reader task: read WS frames and pipe into the Connection's reader ---
    let reader_handle = tokio::spawn(async move {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    // Write the line + \n so BufReader::read_line works.
                    let mut line = text.to_string();
                    if !line.ends_with('\n') {
                        line.push('\n');
                    }
                    if pipe_writer.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    // axum handles ping/pong automatically.
                }
                Ok(Message::Binary(_)) => {
                    debug!(client_id = %id, "ignoring binary WS frame");
                }
                Err(e) => {
                    debug!(client_id = %id, error = %e, "WS read error");
                    break;
                }
            }
        }
        // Close the pipe so the Connection sees EOF.
        drop(pipe_writer);
    });

    // --- Run the IRC Connection over the pipe reader + mpsc sender ---
    let conn = Connection::new(id, (*state).clone(), hostname);
    conn.run_generic(BufReader::new(pipe_reader), tx).await;

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
