use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use super::{reject_disallowed_claude_options, AppState};
use crate::error::ErrorResponse;
use crate::options::ClaudeAgentOptions;

#[derive(Deserialize)]
pub struct QueryRequest {
    pub prompt: String,
    #[serde(default)]
    pub options: Option<ClaudeAgentOptions>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/query", post(query_handler))
        .route("/query/stream", post(query_stream_handler))
}

async fn query_handler(State(state): State<AppState>, Json(req): Json<QueryRequest>) -> Response {
    let options = req.options.unwrap_or_default();
    if let Some(rejected) = reject_disallowed_claude_options(&state, &options) {
        return rejected;
    }

    // Track stats
    {
        let mut stats = state.stats.lock().await;
        stats.total_queries += 1;
    }

    match crate::query::query(&req.prompt, options, &state.config).await {
        Ok(result) => {
            // A stateless query owns its CLI session, so the reported
            // running total is this call's cost.
            let mut seen = 0.0;
            let cost = crate::core::stats::cost_delta(&mut seen, result.total_cost_usd);
            state
                .stats
                .lock()
                .await
                .record_turn(result.usage.as_ref(), cost);
            Json(result).into_response()
        }
        Err(e) => {
            let status = axum::http::StatusCode::from_u16(e.http_status())
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(ErrorResponse::from(&e))).into_response()
        }
    }
}

async fn query_stream_handler(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Response {
    let options = req.options.unwrap_or_default();
    if let Some(rejected) = reject_disallowed_claude_options(&state, &options) {
        return rejected;
    }

    {
        let mut stats = state.stats.lock().await;
        stats.total_queries += 1;
    }

    match crate::query::query_stream(&req.prompt, options, &state.config).await {
        Ok(mut msg_rx) => {
            let stats = state.stats.clone();
            let stream = async_stream::stream! {
                // One CLI session per call, so the running total starts at 0.
                let mut seen_cost = 0.0;
                while let Some(msg) = msg_rx.recv().await {
                    if let crate::messages::Message::Result { usage, total_cost_usd, .. } = &msg {
                        let cost = crate::core::stats::cost_delta(&mut seen_cost, *total_cost_usd);
                        stats.lock().await.record_turn(usage.as_ref(), cost);
                    }
                    match serde_json::to_string(&msg) {
                        Ok(data) => yield Ok::<_, axum::Error>(Event::default().data(data)),
                        Err(_) => continue,
                    }
                }
                yield Ok(Event::default().data("[DONE]"));
            };
            Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
        Err(e) => {
            let status = axum::http::StatusCode::from_u16(e.http_status())
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(ErrorResponse::from(&e))).into_response()
        }
    }
}
