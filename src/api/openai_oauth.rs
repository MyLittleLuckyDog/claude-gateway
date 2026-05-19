use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::StreamExt;
use serde_json::json;

use super::AppState;
use crate::api::error_response;
use crate::openai_oauth;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/openai-oauth/v1/responses", post(responses_handler))
        .route("/openai-oauth/v1/responses/stream", post(responses_stream_handler))
        .route("/openai-oauth/v1/chat/completions", post(chat_completions_handler))
        .route("/openai-oauth/v1/models", get(models_handler))
        .route("/openai-oauth/v1/proxy_stats", get(stats_handler))
}

fn validate_responses_request(body: &serde_json::Value) -> Option<Response> {
    let obj = match body.as_object() {
        Some(obj) => obj,
        None => {
            return Some(error_response(400, "invalid_request", "Request body must be a JSON object"))
        }
    };
    if !obj.contains_key("model") {
        return Some(error_response(400, "invalid_request", "Missing required field: model"));
    }
    if !obj.contains_key("input") {
        return Some(error_response(400, "invalid_request", "Missing required field: input"));
    }
    None
}

fn validate_chat_request(body: &serde_json::Value) -> Option<Response> {
    let obj = match body.as_object() {
        Some(obj) => obj,
        None => {
            return Some(error_response(400, "invalid_request", "Request body must be a JSON object"))
        }
    };
    if !obj.contains_key("model") {
        return Some(error_response(400, "invalid_request", "Missing required field: model"));
    }
    if !obj.contains_key("messages") {
        return Some(error_response(400, "invalid_request", "Missing required field: messages"));
    }
    None
}

async fn responses_handler(
    State(state): State<AppState>,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    let oauth = match state.openai_oauth.as_ref() {
        Some(s) => s,
        None => {
            return error_response(
                501,
                "openai_oauth_disabled",
                "OpenAI OAuth channel not available (auth.json not found)",
            )
        }
    };

    if let Some(err) = validate_responses_request(&body) {
        return err;
    }

    let is_stream = body
        .as_object()
        .and_then(|o| o.get("stream"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }
    let body = openai_oauth::normalize_codex_body(body);

    if is_stream {
        match openai_oauth::responses_stream(oauth, body).await {
            Ok((resp, upstream_status)) => {
                if upstream_status != 200 {
                    let body = resp.json::<serde_json::Value>().await.unwrap_or_else(|_| {
                        json!({"error": {"type": "unknown", "message": "Unknown upstream error"}})
                    });
                    let status = StatusCode::from_u16(upstream_status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    return (status, Json(body)).into_response();
                }

                let byte_stream = resp.bytes_stream().map(|chunk| {
                    chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                });
                let body = axum::body::Body::from_stream(byte_stream);

                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .header("connection", "keep-alive")
                    .body(body)
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
            }
        }
    } else {
        match openai_oauth::responses_sync(oauth, body).await {
            Ok((resp_body, upstream_status)) => {
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(resp_body)).into_response()
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
            }
        }
    }
}

async fn responses_stream_handler(
    State(state): State<AppState>,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    let oauth = match state.openai_oauth.as_ref() {
        Some(s) => s,
        None => {
            return error_response(
                501,
                "openai_oauth_disabled",
                "OpenAI OAuth channel not available (auth.json not found)",
            )
        }
    };

    if let Some(err) = validate_responses_request(&body) {
        return err;
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let body = openai_oauth::normalize_codex_body(body);

    match openai_oauth::responses_stream(oauth, body).await {
        Ok((resp, upstream_status)) => {
            if upstream_status != 200 {
                let body = resp.json::<serde_json::Value>().await.unwrap_or_else(|_| {
                    json!({"error": {"type": "unknown", "message": "Unknown upstream error"}})
                });
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(body)).into_response();
            }

            // Pass SSE stream through, scanning for event: error lines
            let byte_stream = resp.bytes_stream().map(|chunk| {
                chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            });
            let body = axum::body::Body::from_stream(byte_stream);

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            let status = StatusCode::from_u16(e.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
        }
    }
}

async fn chat_completions_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let oauth = match state.openai_oauth.as_ref() {
        Some(s) => s,
        None => {
            return error_response(
                501,
                "openai_oauth_disabled",
                "OpenAI OAuth channel not available (auth.json not found)",
            )
        }
    };

    if let Some(err) = validate_chat_request(&body) {
        return err;
    }

    // Detect streaming request
    let is_stream = body
        .as_object()
        .and_then(|o| o.get("stream"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_stream {
        let request_model = body["model"].as_str().unwrap_or("gpt-5.4-mini").to_string();
        match openai_oauth::chat_completions_stream(oauth, body).await {
            Ok((resp, upstream_status)) => {
                if upstream_status != 200 {
                    let body = resp.json::<serde_json::Value>().await.unwrap_or_else(|_| {
                        json!({"error": {"type": "unknown", "message": "Unknown upstream error"}})
                    });
                    let status = StatusCode::from_u16(upstream_status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    return (status, Json(body)).into_response();
                }

                let stream_body = openai_oauth::chat_sse_body(resp, request_model);

                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .header("connection", "keep-alive")
                    .body(stream_body)
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
            }
        }
    } else {
        match openai_oauth::chat_completions_sync(oauth, body).await {
            Ok((resp_body, upstream_status)) => {
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(resp_body)).into_response()
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
            }
        }
    }
}

async fn models_handler(State(state): State<AppState>) -> Response {
    let oauth = match state.openai_oauth.as_ref() {
        Some(s) => s,
        None => {
            return error_response(
                501,
                "openai_oauth_disabled",
                "OpenAI OAuth channel not available (auth.json not found)",
            )
        }
    };

    match openai_oauth::models(oauth).await {
        Ok((resp_body, upstream_status)) => {
            let status = StatusCode::from_u16(upstream_status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(resp_body)).into_response()
        }
        Err(e) => {
            let status = StatusCode::from_u16(e.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
        }
    }
}

async fn stats_handler(State(state): State<AppState>) -> Response {
    let oauth = match state.openai_oauth.as_ref() {
        Some(s) => s,
        None => {
            return error_response(
                501,
                "openai_oauth_disabled",
                "OpenAI OAuth channel not available (auth.json not found)",
            )
        }
    };

    Json(json!({
        "total_requests": oauth.total_requests.load(Ordering::Relaxed),
        "base_url": oauth.base_url,
    }))
    .into_response()
}
