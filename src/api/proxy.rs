//! HTTP handlers for the direct Messages API proxy.
//!
//! Endpoints:
//!   POST /v1/messages       — sync response (passthrough to Anthropic API);
//!                             `stream: true` is served as SSE, as the SDKs expect
//!   POST /v1/messages/stream — SSE streaming (passthrough)
//!   POST /v1/messages/count_tokens — counted upstream (501 where it cannot be)
//!   GET  /v1/models[/:id]   — model list / limits from upstream (404 without the proxy)
//!   GET  /v1/rate_limit     — current rate limit status

use std::sync::atomic::Ordering;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

/// Resolve the upstream session_id from request headers, falling back to
/// a fresh UUID when the client didn't pin one.
///
/// Gateway-internal session ids feed into `inject_metadata` →
/// `metadata.user_id`, and Anthropic's prompt cache appears to key off
/// that user_id (the same prompt prefix issued from two random
/// session_ids consistently produces `cache_read_input_tokens=0`).
///
/// A long-lived caller (cc-gnothi nightly, future sub-agents) that
/// pins a stable id via `X-Session-Id` thus sees its second request hit
/// the cache. Random ids stay the default so legacy callers behave
/// exactly as before.
///
/// `X-Session-Id` is this gateway's own header and wins. Failing that,
/// `X-Claude-Code-Session-Id` -- which Claude Code puts on every request --
/// keeps one CLI conversation under one upstream session.
fn session_id_from(headers: &HeaderMap) -> String {
    ["x-session-id", "x-claude-code-session-id"]
        .iter()
        .find_map(|name| {
            headers
                .get(*name)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
        .unwrap_or_else(crate::client_identity::new_session_id)
}
/// Beta features the caller asked for with the `anthropic-beta` request header
/// — how the SDKs and Claude Code send them. The header may repeat and each
/// line may hold several comma-separated values.
fn request_betas(headers: &HeaderMap) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        let Ok(line) = value.to_str() else { continue };
        for beta in line.split(',').map(str::trim).filter(|b| !b.is_empty()) {
            if !out.iter().any(|seen| seen == beta) {
                out.push(beta.to_string());
            }
        }
    }
    out
}

/// The model family a request is for -- the key of the gateway's rate-limit cache.
fn request_family(body: &serde_json::Value) -> crate::models::ModelFamily {
    crate::models::family_of(body.get("model").and_then(|v| v.as_str()).unwrap_or(""))
}

/// The gateway's older body `betas` field plus the header values, without
/// repeating one. `None` when neither names anything.
fn merge_betas(from_body: Option<Vec<String>>, from_header: Vec<String>) -> Option<Vec<String>> {
    let mut merged = from_body.unwrap_or_default();
    for beta in from_header {
        if !merged.contains(&beta) {
            merged.push(beta);
        }
    }
    (!merged.is_empty()).then_some(merged)
}

use serde_json::json;

use super::{error_response, no_retry, proxy_error_response, with_rate_limit_headers};
use crate::api::AppState;
use crate::proxy;
use crate::{openai_oauth, openai_proxy, transform};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/messages", post(messages_handler))
        .route("/v1/messages/stream", post(messages_stream_handler))
        .route("/v1/messages/count_tokens", post(count_tokens_handler))
        .route("/v1/models", get(models_list_handler))
        .route("/v1/models/:id", get(model_get_handler))
        .route("/v1/rate_limit", get(rate_limit_handler))
        .route("/v1/proxy_stats", get(proxy_stats_handler))
        .route("/v1/auth_status", get(auth_status_handler))
}

/// Validate request body before forwarding. Returns error response or None if valid.
fn validate_request(body: &serde_json::Value) -> Option<Response> {
    let obj = match body.as_object() {
        Some(o) => o,
        None => {
            return Some(error_response(
                400,
                "invalid_request",
                "Request body must be a JSON object",
            ))
        }
    };

    // model is required
    if !obj.contains_key("model") {
        return Some(error_response(
            400,
            "invalid_request",
            "Missing required field: model",
        ));
    }

    // messages is required and must be non-empty array
    match obj.get("messages") {
        None => {
            return Some(error_response(
                400,
                "invalid_request",
                "Missing required field: messages",
            ))
        }
        Some(v) => {
            if !v.is_array() {
                return Some(error_response(
                    400,
                    "invalid_request",
                    "messages must be an array",
                ));
            }
            if v.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                return Some(error_response(
                    400,
                    "invalid_request",
                    "messages must not be empty",
                ));
            }
        }
    }

    // max_tokens sanity check
    if let Some(mt) = obj.get("max_tokens") {
        if let Some(n) = mt.as_u64() {
            if n == 0 {
                return Some(error_response(
                    400,
                    "invalid_request",
                    "max_tokens must be > 0",
                ));
            }
            if n > 128_000 {
                return Some(error_response(
                    400,
                    "invalid_request",
                    "max_tokens exceeds maximum (128000)",
                ));
            }
        }
    }

    None
}

/// Common request preprocessing: validate, extract betas, normalize model,
/// default max_tokens. Returns (forward_body, extra_betas) or an error Response.
#[allow(clippy::result_large_err)]
fn preprocess_request(
    body: &serde_json::Value,
) -> Result<(serde_json::Value, Option<Vec<String>>), Response> {
    if let Some(err) = validate_request(body) {
        return Err(err);
    }

    let extra_betas = body.get("betas").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect::<Vec<_>>()
    });

    let mut forward_body = body.clone();
    if let Some(obj) = forward_body.as_object_mut() {
        obj.remove("betas");
        obj.remove("stream");

        let model_str = obj.get("model").and_then(|v| v.as_str()).map(String::from);
        if let Some(model_val) = model_str {
            // `canonical_model_id` passes an unrecognized string through
            // unchanged rather than guessing a substitute (never force a
            // model switch the caller didn't ask for) — but a silent
            // passthrough means a typo'd or retired model id in the
            // caller's own config reaches Anthropic unnoticed and comes
            // back as an opaque 400. Surface it here instead.
            if crate::models::resolve_model(&model_val).is_none() {
                tracing::warn!(
                    "[model-catalog-miss] caller requested unrecognized model \"{}\" — \
                     passing through unchanged, not substituting a different model",
                    model_val,
                );
            }
            let canonical = crate::models::canonical_model_id(&model_val).to_string();
            let needs_max_tokens = !obj.contains_key("max_tokens");
            let default_mt = crate::models::default_max_tokens(&canonical);
            obj.insert("model".to_string(), serde_json::Value::String(canonical));
            if needs_max_tokens {
                obj.insert(
                    "max_tokens".to_string(),
                    serde_json::Value::Number(default_mt.into()),
                );
            }
        }
    }

    Ok((forward_body, extra_betas))
}

/// POST /v1/messages — synchronous Messages API proxy
async fn messages_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // The Anthropic SDKs and Claude Code always send `stream: true` to this
    // very path and expect SSE back; `/v1/messages/stream` is only this
    // gateway's own spelling of the same thing.
    if body.get("stream").and_then(serde_json::Value::as_bool) == Some(true) {
        return messages_stream_handler(State(state), headers, Json(body)).await;
    }

    // Resolve model alias before routing
    let body = resolve_model_alias(&state, body);

    // Check if this is an OpenAI model — route to ChatCompletion channel
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if transform::is_openai_model(model) {
        return handle_openai_messages(&state, body).await;
    }

    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let (forward_body, body_betas) = match preprocess_request(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let extra_betas = merge_betas(body_betas, request_betas(&headers));

    let session_id = session_id_from(&headers);
    let family = request_family(&forward_body);

    match proxy::messages_sync(
        proxy_state,
        forward_body,
        extra_betas.as_deref(),
        &session_id,
    )
    .await
    {
        Ok((resp_body, upstream_status)) => {
            let status =
                StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp = (status, Json(resp_body)).into_response();
            if upstream_status == 429 {
                return with_rate_limit_headers(
                    resp,
                    proxy_state.rejection_expires_at(family).await,
                );
            }
            resp
        }
        Err(e) => proxy_error_response(e),
    }
}

/// Route an Anthropic-format `/v1/messages` request to the OpenAI ChatCompletion channel.
async fn handle_openai_messages(state: &AppState, body: serde_json::Value) -> Response {
    if let Some(err) = validate_request(&body) {
        return err;
    }

    let openai_body = transform::anthropic_to_openai_chat(&body);

    let result: Result<(serde_json::Value, u16), crate::error::GatewayError> =
        if let Some(proxy_state) = state.openai.as_ref() {
            match openai_proxy::chat_completions(proxy_state, openai_body).await {
                Ok((resp, status)) => match resp.json::<serde_json::Value>().await {
                    Ok(json) => Ok((json, status)),
                    Err(e) => Err(crate::error::GatewayError::Internal(format!(
                        "OpenAI chat/completions JSON decode failed: {e}"
                    ))),
                },
                Err(e) => Err(e),
            }
        } else if let Some(oauth) = state.openai_oauth.as_ref() {
            openai_oauth::chat_completions_sync(oauth, openai_body).await
        } else {
            return error_response(
                501,
                "openai_proxy_disabled",
                "OpenAI proxy is not enabled (set OPENAI_API_KEY or provide auth.json)",
            );
        };

    match result {
        Ok((openai_resp, upstream_status)) => {
            let status =
                StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if upstream_status != 200 {
                // Pass upstream error through as-is
                return (status, Json(openai_resp)).into_response();
            }
            let anthropic_resp = transform::openai_chat_to_anthropic(&openai_resp);
            (status, Json(anthropic_resp)).into_response()
        }
        Err(e) => {
            let status =
                StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
        }
    }
}

/// Route an Anthropic-format `/v1/messages/stream` request to the OpenAI ChatCompletion streaming channel.
/// Image blocks are converted via `anthropic_to_openai_chat`. The raw OpenAI SSE stream is passed through.
async fn handle_openai_messages_stream(state: &AppState, body: serde_json::Value) -> Response {
    if let Some(err) = validate_request(&body) {
        return err;
    }

    let mut openai_body = transform::anthropic_to_openai_chat(&body);
    if let Some(obj) = openai_body.as_object_mut() {
        obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let raw_result: Result<(reqwest::Response, u16), crate::error::GatewayError> =
        if let Some(proxy_state) = state.openai.as_ref() {
            openai_proxy::chat_completions(proxy_state, openai_body).await
        } else if let Some(oauth) = state.openai_oauth.as_ref() {
            openai_oauth::chat_completions_stream(oauth, openai_body).await
        } else {
            return error_response(
                501,
                "openai_proxy_disabled",
                "OpenAI proxy is not enabled (set OPENAI_API_KEY or provide auth.json)",
            );
        };

    match raw_result {
        Ok((resp, upstream_status)) => {
            if upstream_status != 200 {
                let body = proxy::error_body(resp).await;
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(body)).into_response();
            }

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
        Err(e) => {
            let status =
                StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(crate::error::ErrorResponse::from(&e))).into_response()
        }
    }
}

fn resolve_model_alias(state: &AppState, mut body: serde_json::Value) -> serde_json::Value {
    if let Some(aliases) = state.config.model_aliases.as_ref() {
        if let Some(model) = body.get("model").and_then(|v| v.as_str()).map(String::from) {
            if let Some(resolved) = aliases.get(&model) {
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "model".to_string(),
                        serde_json::Value::String(resolved.clone()),
                    );
                }
            }
        }
    }
    body
}

/// POST /v1/messages/count_tokens — counted by the upstream API.
///
/// The request is the Messages body the caller intends to send (model aliases
/// and the `anthropic-beta` header are honored as on `/v1/messages`); only the
/// fields upstream's counter accepts are forwarded. The count leaves out the
/// billing marker the gateway adds to real requests, since the server does not
/// bill it either.
///
/// Where it cannot be served -- no direct API proxy, or a non-Claude model --
/// the answer is `501 not_supported` with `x-should-retry: false`, the
/// protocol's cue for the client to count tokens itself.
async fn count_tokens_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let not_supported = |why: &str| no_retry(error_response(501, "not_supported", why));

    let body = resolve_model_alias(&state, body);
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if transform::is_openai_model(model) {
        return not_supported("Token counting is only available for Claude models");
    }
    let Some(proxy_state) = state.proxy.as_ref() else {
        return not_supported("Token counting needs the direct API proxy, which is not enabled");
    };

    let (forward_body, body_betas) = match preprocess_request(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let extra_betas = merge_betas(body_betas, request_betas(&headers));
    let session_id = session_id_from(&headers);

    match proxy::count_tokens(
        proxy_state,
        &forward_body,
        extra_betas.as_deref(),
        &session_id,
    )
    .await
    {
        Ok((resp_body, upstream_status)) => {
            let status =
                StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(resp_body)).into_response()
        }
        Err(e) => proxy_error_response(e),
    }
}

/// Characters a model id may contain. The id becomes part of the upstream URL,
/// so anything else (`/`, `?`, `#`, spaces, `..`) is refused outright.
fn is_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
        && !id.contains("..")
}

/// The pagination parameters of the models list, and nothing else.
fn models_query(raw: Option<&str>) -> Option<String> {
    let kept: Vec<&str> = raw?
        .split('&')
        .filter(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            matches!(key, "limit" | "before_id" | "after_id")
                && !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("&"))
}

/// Answer a models request from upstream; without the direct API proxy there is
/// nothing to ask, and `404` is the protocol's cue to use the built-in list.
async fn models_from_upstream(state: &AppState, path_and_query: &str) -> Response {
    let Some(proxy_state) = state.proxy.as_ref() else {
        return error_response(
            404,
            "not_found_error",
            "The model list needs the direct API proxy, which is not enabled",
        );
    };
    match proxy::upstream_get(proxy_state, path_and_query).await {
        Ok((body, upstream_status)) => {
            let status =
                StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(body)).into_response()
        }
        Err(e) => proxy_error_response(e),
    }
}

/// GET /v1/models — the models upstream offers this account.
async fn models_list_handler(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    let path = match models_query(query.as_deref()) {
        Some(q) => format!("/v1/models?{q}"),
        None => "/v1/models".to_string(),
    };
    models_from_upstream(&state, &path).await
}

/// GET /v1/models/:id — one model, with its token limits.
async fn model_get_handler(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !is_model_id(&id) {
        return error_response(400, "invalid_request_error", "Not a valid model id");
    }
    models_from_upstream(&state, &format!("/v1/models/{id}")).await
}

/// POST /v1/messages/stream — SSE streaming Messages API proxy
async fn messages_stream_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // Resolve model alias before routing
    let body = resolve_model_alias(&state, body);

    // Check if this is an OpenAI model — route to ChatCompletion streaming channel
    let model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if transform::is_openai_model(model) {
        return handle_openai_messages_stream(&state, body).await;
    }

    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let (forward_body, body_betas) = match preprocess_request(&body) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let extra_betas = merge_betas(body_betas, request_betas(&headers));

    let session_id = session_id_from(&headers);
    let family = request_family(&forward_body);

    match proxy::messages_stream(
        proxy_state,
        forward_body,
        extra_betas.as_deref(),
        &session_id,
    )
    .await
    {
        Ok((resp, upstream_status)) => {
            if upstream_status != 200 {
                // Non-200: read body as JSON error
                let body = proxy::error_body(resp).await;
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let resp = (status, Json(body)).into_response();
                if upstream_status == 429 {
                    return with_rate_limit_headers(
                        resp,
                        proxy_state.rejection_expires_at(family).await,
                    );
                }
                return resp;
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
async fn rate_limit_handler(State(state): State<AppState>) -> Response {
    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    // Keyed by model family (Haiku/Sonnet/Opus/Fable each have their own
    // quota pool on the real API — confirmed live 2026-10-24) rather than
    // one shared status, so e.g. Fable's real 0%-utilized state is visible
    // even while Sonnet traffic has its own family pegged near its limit.
    let map = proxy_state.rate_limit.read().await;
    let by_family: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .map(|(family, rl)| {
            (
                format!("{family:?}"),
                json!({
                    "status": rl.status,
                    "utilization_5h": rl.utilization_5h,
                    "utilization_7d": rl.utilization_7d,
                    "resets_at": rl.resets_at,
                    "fallback_available": rl.fallback_available,
                    "rate_limit_type": rl.rate_limit_type,
                    "overage_status": rl.overage_status,
                    "overage_disabled_reason": rl.overage_disabled_reason,
                }),
            )
        })
        .collect();
    Json(json!({ "by_family": by_family })).into_response()
}

/// GET /v1/proxy_stats — proxy usage statistics
async fn proxy_stats_handler(State(state): State<AppState>) -> Response {
    let proxy_state = match state.proxy.as_ref() {
        Some(ps) => ps,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    Json(json!({
        "total_requests": proxy_state.total_requests.load(Ordering::Relaxed),
        "total_input_tokens": proxy_state.total_input_tokens.load(Ordering::Relaxed),
        "total_output_tokens": proxy_state.total_output_tokens.load(Ordering::Relaxed),
        "concurrent_permits_available": proxy_state.semaphore.available_permits(),
    }))
    .into_response()
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
            }))
            .into_response()
        }
        Err(msg) => Json(json!({
            "authenticated": false,
            "error": msg,
        }))
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_allow_only_id_characters() {
        for ok in [
            "claude-sonnet-5-5",
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
            "a.b_c:d",
        ] {
            assert!(is_model_id(ok), "{ok}");
        }
        for bad in [
            "",
            "a/b",
            "../x",
            "a..b",
            "a?b=c",
            "a#b",
            "a b",
            "a%2Fb",
            &"x".repeat(129),
        ] {
            assert!(!is_model_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn models_query_forwards_only_pagination() {
        assert_eq!(models_query(None), None);
        assert_eq!(models_query(Some("")), None);
        assert_eq!(
            models_query(Some("limit=5&evil=1&after_id=model-1")).as_deref(),
            Some("limit=5&after_id=model-1")
        );
        // Empty or non-id values are dropped, so nothing can be smuggled in.
        assert_eq!(models_query(Some("limit=")), None);
        assert_eq!(models_query(Some("limit=5%26x%3D1")), None);
        assert_eq!(models_query(Some("before_id=a/b")), None);
    }
}
