//! HTTP handlers for the direct Messages API proxy.
//!
//! Endpoints:
//!   POST /v1/messages       — sync response (passthrough to Anthropic API)
//!   POST /v1/messages/stream — SSE streaming (passthrough)
//!   GET  /v1/rate_limit     — current rate limit status

use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;

use crate::api::AppState;
use crate::proxy::{self, ProxyError};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/messages", post(messages_handler))
        .route("/v1/messages/stream", post(messages_stream_handler))
        .route("/v1/rate_limit", get(rate_limit_handler))
        .route("/v1/proxy_stats", get(proxy_stats_handler))
        .route("/v1/auth_status", get(auth_status_handler))
}

/// POST /v1/messages — synchronous Messages API proxy
async fn messages_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    // Extract optional beta headers from body
    let extra_betas = body.get("betas")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>());

    // Remove gateway-only fields and normalize before forwarding
    let mut forward_body = body.clone();
    if let Some(obj) = forward_body.as_object_mut() {
        obj.remove("betas");
        obj.remove("stream");

        // Normalize model alias → canonical ID + default max_tokens
        let model_str = obj.get("model").and_then(|v| v.as_str()).map(String::from);
        if let Some(model_val) = model_str {
            let canonical = crate::models::canonical_model_id(&model_val).to_string();
            let needs_max_tokens = !obj.contains_key("max_tokens");
            let default_mt = crate::models::default_max_tokens(&canonical);
            obj.insert("model".to_string(), serde_json::Value::String(canonical));
            if needs_max_tokens {
                obj.insert("max_tokens".to_string(), serde_json::Value::Number(default_mt.into()));
            }
        }
    }

    match proxy::messages_sync(
        proxy_state,
        forward_body,
        extra_betas.as_deref(),
    ).await {
        Ok((resp_body, upstream_status)) => {
            let status = StatusCode::from_u16(upstream_status)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(resp_body)).into_response()
        }
        Err(e) => proxy_error_response(e),
    }
}

/// POST /v1/messages/stream — SSE streaming Messages API proxy
async fn messages_stream_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let extra_betas = body.get("betas")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>());

    let mut forward_body = body.clone();
    if let Some(obj) = forward_body.as_object_mut() {
        obj.remove("betas");

        // Normalize model alias → canonical ID + default max_tokens
        let model_str = obj.get("model").and_then(|v| v.as_str()).map(String::from);
        if let Some(model_val) = model_str {
            let canonical = crate::models::canonical_model_id(&model_val).to_string();
            let needs_max_tokens = !obj.contains_key("max_tokens");
            let default_mt = crate::models::default_max_tokens(&canonical);
            obj.insert("model".to_string(), serde_json::Value::String(canonical));
            if needs_max_tokens {
                obj.insert("max_tokens".to_string(), serde_json::Value::Number(default_mt.into()));
            }
        }
    }

    match proxy::messages_stream(
        proxy_state,
        forward_body,
        extra_betas.as_deref(),
    ).await {
        Ok((resp, upstream_status)) => {
            if upstream_status != 200 {
                // Non-200: read body as JSON error
                let body = resp.json::<serde_json::Value>().await
                    .unwrap_or_else(|_| json!({"error": {"type": "unknown", "message": "Unknown upstream error"}}));
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(body)).into_response();
            }

            // 200: Raw SSE passthrough — upstream already sends proper SSE format
            let byte_stream = resp.bytes_stream();
            let body = axum::body::Body::from_stream(byte_stream);

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("connection", "keep-alive")
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => proxy_error_response(e),
    }
}

/// GET /v1/rate_limit — current rate limit status
async fn rate_limit_handler(
    State(state): State<AppState>,
) -> Response {
    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let rl = proxy_state.rate_limit.read().await;
    Json(json!({
        "status": rl.status,
        "utilization_5h": rl.utilization_5h,
        "utilization_7d": rl.utilization_7d,
        "resets_at": rl.resets_at,
        "fallback_available": rl.fallback_available,
    })).into_response()
}

/// GET /v1/proxy_stats — proxy usage statistics
async fn proxy_stats_handler(
    State(state): State<AppState>,
) -> Response {
    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    Json(json!({
        "total_requests": proxy_state.total_requests.load(Ordering::Relaxed),
        "total_input_tokens": proxy_state.total_input_tokens.load(Ordering::Relaxed),
        "total_output_tokens": proxy_state.total_output_tokens.load(Ordering::Relaxed),
        "concurrent_permits_available": proxy_state.semaphore.available_permits(),
    })).into_response()
}

/// GET /v1/auth_status — check OAuth token status
async fn auth_status_handler() -> Response {
    match crate::auth::get_oauth_token() {
        Ok(token) => {
            let valid = crate::auth::is_token_valid(&token);
            Json(json!({
                "authenticated": true,
                "token_valid": valid,
                "subscription_type": token.subscription_type,
                "rate_limit_tier": token.rate_limit_tier,
                "expires_at": token.expires_at,
            })).into_response()
        }
        Err(msg) => {
            Json(json!({
                "authenticated": false,
                "error": msg,
            })).into_response()
        }
    }
}

fn error_response(status: u16, code: &str, message: &str) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status_code, Json(json!({
        "error": {
            "type": code,
            "message": message,
        }
    }))).into_response()
}

fn proxy_error_response(e: ProxyError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(json!({
        "error": {
            "type": e.error_code(),
            "message": e.to_string(),
        }
    }))).into_response()
}
