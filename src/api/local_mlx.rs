use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;

use super::{error_response, AppState};
use crate::local_mlx_proxy;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/gemma/v1/chat/completions", post(chat_completions_handler))
        .route("/gemma/v1/models", get(models_handler))
        .route("/gemma/v1/proxy_stats", get(stats_handler))
        .route(
            "/local-mlx/v1/chat/completions",
            post(chat_completions_handler),
        )
        .route("/local-mlx/v1/models", get(models_handler))
        .route("/local-mlx/v1/proxy_stats", get(stats_handler))
}

fn validate_chat_request(body: &serde_json::Value) -> Option<Response> {
    let obj = match body.as_object() {
        Some(obj) => obj,
        None => {
            return Some(error_response(
                400,
                "invalid_request",
                "Request body must be a JSON object",
            ))
        }
    };

    match obj.get("messages") {
        Some(messages) if messages.as_array().is_some_and(|m| !m.is_empty()) => {}
        Some(_) => {
            return Some(error_response(
                400,
                "invalid_request",
                "messages must be a non-empty array",
            ))
        }
        None => {
            return Some(error_response(
                400,
                "invalid_request",
                "Missing required field: messages",
            ))
        }
    }

    if obj.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        return Some(error_response(
            400,
            "unsupported_request",
            "Gemma streaming is not supported by this gateway endpoint yet",
        ));
    }

    None
}

async fn chat_completions_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(err) = validate_chat_request(&body) {
        return err;
    }

    let local_mlx = match state.local_mlx.as_ref() {
        Some(local_mlx) => local_mlx,
        None => {
            return error_response(
                501,
                "gemma_disabled",
                "Gemma proxy is disabled (set GEMMA_ENABLED=true or unset it)",
            )
        }
    };

    match local_mlx_proxy::chat_completions_sync(local_mlx, body).await {
        Ok((resp_body, upstream_status)) => {
            let status =
                StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(resp_body)).into_response()
        }
        Err(e) => {
            let status =
                StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
        }
    }
}

async fn models_handler(State(state): State<AppState>) -> Response {
    let local_mlx = match state.local_mlx.as_ref() {
        Some(local_mlx) => local_mlx,
        None => return error_response(501, "gemma_disabled", "Gemma proxy is disabled"),
    };

    Json(local_mlx_proxy::models_json(local_mlx)).into_response()
}

async fn stats_handler(State(state): State<AppState>) -> Response {
    let local_mlx = match state.local_mlx.as_ref() {
        Some(local_mlx) => local_mlx,
        None => return error_response(501, "gemma_disabled", "Gemma proxy is disabled"),
    };

    Json(json!({
        "provider": "gemma",
        "backend": "local-mlx",
        "base_url": local_mlx.base_url,
        "model": local_mlx.model,
        "alias": local_mlx.alias,
        "min_max_tokens": local_mlx.min_max_tokens,
        "total_requests": local_mlx.total_requests.load(Ordering::Relaxed),
    }))
    .into_response()
}
