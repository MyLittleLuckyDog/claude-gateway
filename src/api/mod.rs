pub mod admin;
pub mod hooks;
pub mod proxy;
pub mod proxy_sessions;
pub mod query;
pub mod sessions;

use std::sync::Arc;
use axum::Router;
use tower_http::cors::CorsLayer;

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

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .merge(admin::routes())
        .merge(query::routes())
        .merge(sessions::routes())
        .merge(hooks::routes())
        .merge(proxy::routes())
        .merge(proxy_sessions::routes())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
