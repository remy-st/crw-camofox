//! crw-browser-bridge — consolidates the interactive subset of the
//! camofox-browser REST API (navigate/snapshot/click/type/.../download, 13
//! actions total) behind a single `POST /browser` action-dispatch endpoint,
//! and owns idle-tab cleanup so callers never have to.
//!
//! Why this exists: camofox-browser's own idle reaper only fires once the
//! *whole server* has zero live tabs, and a tab a caller forgets to close
//! blocks that reaper indefinitely (see `reaper.rs`). Its would-be primary
//! caller here is a TUI coding agent (Pi) with no reliable shutdown hook —
//! it can be killed, crash, or have its terminal closed without ever running
//! cleanup code. So this bridge tracks last-activity per `userId` itself and
//! force-closes anything idle past a configurable threshold, independent of
//! any client's lifecycle.
//!
//! One tab per `userId` — callers mint a stable id per conversation/process
//! and every action after `navigate` implicitly targets that tab. No
//! tab-listing or multi-tab surface; that's what the full camofox-mcp server
//! is for if you need it.

mod actions;
mod camofox;
mod reaper;
mod state;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde_json::json;
use tracing_subscriber::EnvFilter;

use camofox::CamofoxClient;
use state::AppState;

#[derive(Parser)]
#[command(name = "crw-browser-bridge", about = "Idle-reaping REST bridge for camofox-browser")]
struct Cli {
    /// Address to bind the bridge's own HTTP server on.
    #[arg(long, env = "CAMOFOX_BRIDGE_HOST", default_value = "0.0.0.0")]
    host: String,

    /// Port for the bridge's own HTTP server.
    #[arg(long, env = "CAMOFOX_BRIDGE_PORT", default_value_t = 9379)]
    port: u16,

    /// Base URL of the upstream camofox-browser REST server.
    #[arg(long, env = "CAMOFOX_URL", default_value = "http://camofox:9377")]
    camofox_url: String,

    /// Bearer key for the upstream camofox-browser server, if it requires one.
    #[arg(long, env = "CAMOFOX_API_KEY")]
    camofox_api_key: Option<String>,

    /// Bearer key callers of THIS bridge must present. Unset disables auth —
    /// only safe when the bridge is bound to loopback / a private network.
    #[arg(long, env = "CAMOFOX_BRIDGE_API_KEY")]
    bridge_api_key: Option<String>,

    /// Minutes of inactivity before an idle browser session is force-closed.
    #[arg(long, env = "CAMOFOX_BRIDGE_IDLE_TIMEOUT_MINUTES", default_value_t = 30)]
    idle_timeout_minutes: u64,

    /// Timeout for each individual upstream camofox-browser HTTP call.
    #[arg(long, env = "CAMOFOX_BRIDGE_UPSTREAM_TIMEOUT_SECS", default_value_t = 30)]
    upstream_timeout_secs: u64,

    /// Host-visible path standing in for wherever camofox's own
    /// `CAMOFOX_DOWNLOADS_DIR` is bind-mounted, so `download` results report
    /// a path the caller can actually open. Files land under
    /// `{this}/{userId}/{filename}` (camofox keys one subdirectory per user).
    #[arg(long, env = "CAMOFOX_BRIDGE_DOWNLOADS_HOST_DIR", default_value = "./downloads")]
    downloads_host_dir: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();

    if cli.bridge_api_key.is_none() {
        tracing::warn!(
            "CAMOFOX_BRIDGE_API_KEY not set — auth is disabled. Only bind this bridge to loopback/a private network."
        );
    }

    let camofox = CamofoxClient::new(
        cli.camofox_url.clone(),
        cli.camofox_api_key.clone(),
        Duration::from_secs(cli.upstream_timeout_secs),
    );

    match camofox.health().await {
        Ok(_) => tracing::info!(camofox_url = %cli.camofox_url, "camofox reachable"),
        Err(e) => tracing::warn!(camofox_url = %cli.camofox_url, error = %e, "camofox not reachable yet (may still be starting)"),
    }

    let state = Arc::new(AppState {
        camofox,
        sessions: dashmap::DashMap::new(),
        idle_timeout: Duration::from_secs(cli.idle_timeout_minutes * 60),
        bridge_api_key: cli.bridge_api_key,
        downloads_host_dir: cli.downloads_host_dir.trim_end_matches('/').to_string(),
    });

    reaper::spawn(state.clone());

    let protected = Router::new()
        .route("/browser", post(actions::handle))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state);

    let addr = format!("{}:{}", cli.host, cli.port);
    tracing::info!("crw-browser-bridge listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("server error: {e}"));
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "ok": true }))
}

async fn require_auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let Some(expected) = &state.bridge_api_key else {
        return Ok(next.run(req).await);
    };
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if provided == Some(expected.as_str()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}
