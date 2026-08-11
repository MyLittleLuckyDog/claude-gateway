pub mod admin;
pub mod codex;
pub mod codex_app;
pub mod hooks;
pub mod local_mlx;
pub mod openai;
pub mod openai_oauth;
pub mod proxy;
pub mod proxy_sessions;
pub mod query;
pub mod sessions;

use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Router;
use serde_json::json;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::codex::store::CodexSessionStore;
use crate::codex_app::store::CodexAppSessionStore;
use crate::config::AppConfig;
use crate::local_mlx_proxy::LocalMlxProxyState;
use crate::openai_oauth::OpenAiOAuthState;
use crate::openai_proxy::OpenAiProxyState;
use crate::proxy::ProxyState;
use crate::proxy_session::ProxySessionStore;
use crate::session::store::SessionStore;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub sessions: Arc<SessionStore>,
    pub codex_sessions: Arc<CodexSessionStore>,
    pub codex_app_sessions: Arc<CodexAppSessionStore>,
    pub start_time: std::time::Instant,
    pub stats: Arc<tokio::sync::Mutex<Stats>>,
    pub proxy: Option<Arc<ProxyState>>,
    pub proxy_sessions: Option<Arc<ProxySessionStore>>,
    pub local_mlx: Option<Arc<LocalMlxProxyState>>,
    pub openai: Option<Arc<OpenAiProxyState>>,
    pub openai_oauth: Option<Arc<OpenAiOAuthState>>,
}

pub use crate::core::stats::Stats;

// ── Shared error helpers ──────────────────────────────────────────

pub fn error_response(status: u16, code: &str, message: &str) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status_code,
        Json(json!({
            "error": {
                "type": code,
                "message": message,
            }
        })),
    )
        .into_response()
}

/// Render a [`GatewayError`](crate::error::GatewayError) as an HTTP response.
///
/// Distinct from [`error_response`], which takes a hand-built status/code/message
/// triple for the provider-proxy routes that have no `GatewayError` to hand.
pub fn gateway_error_response(e: &crate::error::GatewayError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(crate::error::ErrorResponse::from(e))).into_response()
}

pub fn proxy_error_response(e: crate::proxy::ProxyError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(json!({
            "error": {
                "type": e.error_code(),
                "message": e.to_string(),
            }
        })),
    )
        .into_response()
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
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(Any)
}

pub fn build_router(state: AppState) -> Router {
    let cors = build_cors_layer(&state.config.server.cors_origins);
    Router::new()
        .merge(admin::routes())
        .merge(codex::routes())
        .merge(codex_app::routes())
        .merge(openai::routes())
        .merge(openai_oauth::routes())
        .merge(query::routes())
        .merge(sessions::routes())
        .merge(hooks::routes())
        .merge(local_mlx::routes())
        .merge(proxy::routes())
        .merge(proxy_sessions::routes())
        .layer(cors)
        .with_state(state)
}
