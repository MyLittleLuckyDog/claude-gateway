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

use super::AppState;
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

async fn query_handler(
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Response {
    let options = req.options.unwrap_or_default();

    // Track stats
    {
        let mut stats = state.stats.lock().await;
        stats.total_queries += 1;
    }

    match crate::query::query(&req.prompt, options, &state.config).await {
        Ok(result) => {
            // Track token/cost stats from result
            if let Some(ref usage) = result.usage {
                let mut stats = state.stats.lock().await;
                stats.total_input_tokens += usage.input_tokens;
                stats.total_output_tokens += usage.output_tokens;
            }
            if let Some(cost) = result.cost_usd {
                let mut stats = state.stats.lock().await;
                stats.total_cost_usd += cost;
            }
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

    {
        let mut stats = state.stats.lock().await;
        stats.total_queries += 1;
    }

    match crate::query::query_stream(&req.prompt, options, &state.config).await {
        Ok(mut msg_rx) => {
            let stream = async_stream::stream! {
                while let Some(msg) = msg_rx.recv().await {
                    match serde_json::to_string(&msg) {
                        Ok(data) => yield Ok::<_, axum::Error>(Event::default().data(data)),
                        Err(_) => continue,
                    }
                }
                yield Ok(Event::default().data("[DONE]"));
            };
            Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
        }
        Err(e) => {
            let status = axum::http::StatusCode::from_u16(e.http_status())
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(ErrorResponse::from(&e))).into_response()
        }
    }
}
