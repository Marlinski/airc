//! HTTP API server — REST API and Prometheus metrics.
//!
//! Endpoints:
//! - `GET /api/stats`             — server statistics (users, channels, uptime)
//! - `GET /api/channels`          — list of channels with details
//! - `GET /api/reputation/:nick`  — reputation info for a registered nick
//! - `GET /metrics`               — Prometheus exposition format

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use tracing::info;

use crate::state::SharedState;

// Re-export the proto HTTP API types so `state.rs` can reference them
// as `web::StatsResponse` etc. without needing a separate import.
pub use airc_shared::http_api::{
    ChannelInfo, ChannelsResponse, ErrorResponse, ReputationResponse, StatsResponse,
};

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn get_stats(State(state): State<Arc<SharedState>>) -> Json<StatsResponse> {
    let stats = state.api_stats().await;
    Json(stats)
}

async fn get_channels(State(state): State<Arc<SharedState>>) -> Json<ChannelsResponse> {
    let channels = state.api_channels().await;
    Json(ChannelsResponse { channels })
}

async fn get_reputation(
    State(state): State<Arc<SharedState>>,
    Path(nick): Path<String>,
) -> Result<Json<ReputationResponse>, (StatusCode, Json<ErrorResponse>)> {
    match state.api_reputation(&nick).await {
        Some(rep) => Ok(Json(rep)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("{nick} is not a registered nickname."),
            }),
        )),
    }
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
// Router construction
// ---------------------------------------------------------------------------

/// Build the HTTP router with API routes and metrics.
pub fn router(state: SharedState) -> Router {
    let shared = Arc::new(state);

    Router::new()
        .route("/api/stats", get(get_stats))
        .route("/api/channels", get(get_channels))
        .route("/api/reputation/{nick}", get(get_reputation))
        .route("/metrics", get(get_metrics))
        .with_state(shared)
}

/// Start the HTTP server on the given address.
pub async fn serve(addr: &str, state: SharedState) -> std::io::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "HTTP API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
