use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;

use super::AppState;
use crate::api::error_response;
use crate::openai_proxy;
use crate::openai_oauth;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/openai/v1/responses", post(responses_handler))
        .route("/openai/v1/responses/stream", post(responses_stream_handler))
        .route("/openai/v1/models", get(models_handler))
        .route("/openai/v1/proxy_stats", get(stats_handler))
}

fn validate_request(body: &serde_json::Value) -> Option<Response> {
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

    if !obj.contains_key("model") {
        return Some(error_response(400, "invalid_request", "Missing required field: model"));
    }

    if !obj.contains_key("input") {
        return Some(error_response(400, "invalid_request", "Missing required field: input"));
    }

    None
}

async fn responses_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(err) = validate_request(&body) {
        return err;
    }

    if let Some(proxy_state) = state.openai.as_ref() {
        match openai_proxy::responses_sync(proxy_state, body).await {
            Ok((resp_body, upstream_status)) => {
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(resp_body)).into_response();
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(crate::error::ErrorResponse::from(&e))).into_response();
            }
        }
    }

    if let Some(oauth) = state.openai_oauth.as_ref() {
        let body = openai_oauth::normalize_codex_body(body);
        match openai_oauth::responses_sync(oauth, body).await {
            Ok((resp_body, upstream_status)) => {
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(resp_body)).into_response();
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(crate::error::ErrorResponse::from(&e))).into_response();
            }
        }
    }

    error_response(501, "openai_proxy_disabled", "OpenAI proxy is not enabled (set OPENAI_API_KEY or provide auth.json)")
}

async fn responses_stream_handler(
    State(state): State<AppState>,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    if let Some(err) = validate_request(&body) {
        return err;
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    if let Some(proxy_state) = state.openai.as_ref() {
        match openai_proxy::responses_stream(proxy_state, body).await {
            Ok((resp, upstream_status)) => {
                if upstream_status != 200 {
                    let body = resp.json::<serde_json::Value>().await.unwrap_or_else(|_| {
                        json!({"error": {"type": "unknown", "message": "Unknown upstream error"}})
                    });
                    let status = StatusCode::from_u16(upstream_status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    return (status, Json(body)).into_response();
                }

                let byte_stream = resp.bytes_stream();
                let body = axum::body::Body::from_stream(byte_stream);

                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .header("connection", "keep-alive")
                    .body(body)
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(crate::error::ErrorResponse::from(&e))).into_response();
            }
        }
    }

    if let Some(oauth) = state.openai_oauth.as_ref() {
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

                use futures::StreamExt;
                let byte_stream = resp.bytes_stream().map(|chunk| {
                    chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                });
                let body = axum::body::Body::from_stream(byte_stream);

                return Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .header("connection", "keep-alive")
                    .body(body)
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            Err(e) => {
                let status = StatusCode::from_u16(e.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(crate::error::ErrorResponse::from(&e))).into_response();
            }
        }
    }

    error_response(501, "openai_proxy_disabled", "OpenAI proxy is not enabled (set OPENAI_API_KEY or provide auth.json)")
}

async fn models_handler(State(state): State<AppState>) -> Response {
    // Fallback to OAuth channel if API key proxy is not configured
    if state.openai.is_none() {
        if let Some(oauth) = state.openai_oauth.as_ref() {
            match openai_oauth::models(oauth).await {
                Ok((resp_body, upstream_status)) => {
                    let status = StatusCode::from_u16(upstream_status)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    return (status, Json(resp_body)).into_response();
                }
                Err(e) => {
                    let status = StatusCode::from_u16(e.http_status())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    return (status, Json(crate::error::ErrorResponse::from(&e))).into_response();
                }
            }
        }
        return error_response(501, "openai_proxy_disabled", "OpenAI proxy is not enabled (set OPENAI_API_KEY or provide auth.json)");
    }

    let proxy_state = match state.openai.as_ref() {
        Some(ps) => ps,
        None => {
            return error_response(
                501,
                "openai_proxy_disabled",
                "OpenAI proxy is not enabled (set OPENAI_API_KEY)",
            )
        }
    };

    let client = &proxy_state.client;
    let url = format!("{}/v1/models", proxy_state.base_url.trim_end_matches('/'));
    let mut headers = reqwest::header::HeaderMap::new();
    let auth = format!("Bearer {}", proxy_state.api_key);
    let auth = match reqwest::header::HeaderValue::from_str(&auth) {
        Ok(value) => value,
        Err(e) => {
            return error_response(
                500,
                "internal_error",
                &format!("Invalid OPENAI_API_KEY header: {}", e),
            )
        }
    };
    headers.insert(reqwest::header::AUTHORIZATION, auth);

    match client.get(url).headers(headers).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| json!({"error": {"message": "Failed to decode response"}}));
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(body)).into_response()
        }
        Err(e) => error_response(502, "proxy_error", &format!("OpenAI models request failed: {}", e)),
    }
}

async fn stats_handler(State(state): State<AppState>) -> Response {
    let proxy_state = match state.openai.as_ref() {
        Some(ps) => ps,
        None => {
            return error_response(
                501,
                "openai_proxy_disabled",
                "OpenAI proxy is not enabled (set OPENAI_API_KEY)",
            )
        }
    };

    Json(json!({
        "total_requests": proxy_state.total_requests.load(Ordering::Relaxed),
        "base_url": proxy_state.base_url,
    }))
    .into_response()
}
