//! HTTP API server — serves live stats and the static documentation site.
//!
//! Endpoints:
//! - `GET /api/stats`             — server statistics (users, channels, uptime)
//! - `GET /api/channels`          — list of channels with details
//! - `GET /api/reputation/:nick`  — reputation info for a registered nick
//! - `GET /*`                     — static files from the `site/` directory

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use tower_http::services::ServeDir;
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

// ---------------------------------------------------------------------------
// Router construction
// ---------------------------------------------------------------------------

/// Build the complete HTTP router with API routes and static file serving.
pub fn router(state: SharedState, site_dir: &str) -> Router {
    let shared = Arc::new(state);

    let api = Router::new()
        .route("/api/stats", get(get_stats))
        .route("/api/channels", get(get_channels))
        .route("/api/reputation/{nick}", get(get_reputation))
        .with_state(shared);

    // Static file serving for the documentation site.
    let static_files = ServeDir::new(site_dir);

    api.fallback_service(static_files)
}

/// Start the HTTP server on the given address.
pub async fn serve(addr: &str, state: SharedState, site_dir: &str) -> std::io::Result<()> {
    let app = router(state, site_dir);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(addr = %addr, "HTTP API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
