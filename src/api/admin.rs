use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;

use super::AppState;

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    cli_available: bool,
    cli_path: String,
    active_sessions: usize,
    max_sessions: usize,
}

#[derive(Serialize)]
struct StatsResponse {
    uptime_seconds: u64,
    total_queries: u64,
    active_sessions: usize,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cost_usd: f64,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/config", get(get_config))
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let cli_path = find_cli_path();
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        cli_available: cli_path.is_some(),
        cli_path: cli_path.unwrap_or_default(),
        active_sessions: state.sessions.count(),
        max_sessions: state.config.server.max_sessions,
    })
}

async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let stats = state.stats.lock().await;
    Json(StatsResponse {
        uptime_seconds: state.start_time.elapsed().as_secs(),
        total_queries: stats.total_queries,
        active_sessions: state.sessions.count(),
        total_input_tokens: stats.total_input_tokens,
        total_output_tokens: stats.total_output_tokens,
        total_cost_usd: stats.total_cost_usd,
    })
}

async fn get_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "server": {
            "host": state.config.server.host,
            "port": state.config.server.port,
            "max_sessions": state.config.server.max_sessions,
        },
        "cli": {
            "bin_path": state.config.cli.bin_path,
            "session_idle_timeout_secs": state.config.cli.session_idle_timeout_secs,
        }
    }))
}

fn find_cli_path() -> Option<String> {
    std::process::Command::new("which")
        .arg("claude")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}
