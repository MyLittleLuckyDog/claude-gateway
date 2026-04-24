pub mod admin;
pub mod hooks;
pub mod proxy;
pub mod proxy_sessions;
pub mod query;
pub mod sessions;

use std::sync::Arc;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Router;
use serde_json::json;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::config::AppConfig;
use crate::proxy::ProxyState;
use crate::proxy_session::ProxySessionStore;
use crate::session::store::SessionStore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub sessions: Arc<SessionStore>,
    pub start_time: std::time::Instant,
    pub stats: Arc<tokio::sync::Mutex<Stats>>,
    pub proxy: Option<Arc<ProxyState>>,
    pub proxy_sessions: Option<Arc<ProxySessionStore>>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub total_queries: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: f64,
}

// ── Shared error helpers ──────────────────────────────────────────

pub fn error_response(status: u16, code: &str, message: &str) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status_code, Json(json!({
        "error": {
            "type": code,
            "message": message,
        }
    }))).into_response()
}

pub fn proxy_error_response(e: crate::proxy::ProxyError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({
        "error": {
            "type": e.error_code(),
            "message": e.to_string(),
        }
    }))).into_response()
}

fn build_cors_layer(origins: &[String]) -> CorsLayer {
    if origins.is_empty() {
        return CorsLayer::permissive();
    }

    // Parse configured origins. For entries like "http://localhost" we also
    // allow any port (the browser sends "http://localhost:3000" etc.).
    let mut allowed: Vec<HeaderValue> = Vec::new();
    for origin in origins {
        if let Ok(val) = HeaderValue::from_str(origin) {
            allowed.push(val);
        }
    }

    if allowed.is_empty() {
        return CorsLayer::permissive();
    }

    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |value, _| {
            let origin = value.to_str().unwrap_or("");
            allowed.iter().any(|a| {
                let prefix = a.to_str().unwrap_or("");
                // Exact match or prefix match with port separator
                origin == prefix || origin.starts_with(&format!("{prefix}:"))
            })
        }))
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE, Method::OPTIONS])
        .allow_headers(Any)
}

pub fn build_router(state: AppState) -> Router {
    let cors = build_cors_layer(&state.config.server.cors_origins);
    Router::new()
        .merge(admin::routes())
        .merge(query::routes())
        .merge(sessions::routes())
        .merge(hooks::routes())
        .merge(proxy::routes())
        .merge(proxy_sessions::routes())
        .layer(cors)
        .with_state(state)
}
