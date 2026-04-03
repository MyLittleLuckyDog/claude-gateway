//! HTTP handlers for proxy-mode multi-turn sessions.
//!
//! POST   /v1/sessions          — create session
//! GET    /v1/sessions          — list sessions (when proxy enabled, merged via query param)
//! POST   /v1/sessions/:id/msg — send message → get response
//! GET    /v1/sessions/:id     — get session state
//! DELETE /v1/sessions/:id     — delete session

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;
use crate::proxy;
use crate::proxy_session::SessionOptions;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/:id", get(get_session))
        .route("/v1/sessions/:id", delete(delete_session))
        .route("/v1/sessions/:id/msg", post(send_message))
        .route("/v1/sessions/:id/msg/stream", post(send_message_stream))
}

// ── Create Session ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateSessionRequest {
    #[serde(flatten)]
    options: SessionOptions,
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    let store = match state.proxy_sessions.as_ref() {
        Some(s) => s,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    match store.create(req.options).await {
        Ok(session) => {
            (StatusCode::CREATED, Json(json!({
                "id": session.id,
                "model": session.model,
                "created_at": session.created_at,
            }))).into_response()
        }
        Err(e) => error_response(429, "session_limit", &e),
    }
}

// ── List Sessions ──────────────────────────────────────────────────

async fn list_sessions(
    State(state): State<AppState>,
) -> Response {
    let store = match state.proxy_sessions.as_ref() {
        Some(s) => s,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let sessions = store.list().await;
    Json(json!({ "sessions": sessions })).into_response()
}

// ── Get Session ────────────────────────────────────────────────────

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let store = match state.proxy_sessions.as_ref() {
        Some(s) => s,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    match store.get(&id).await {
        Some(session) => Json(json!({
            "id": session.id,
            "model": session.model,
            "system": session.system,
            "messages": session.messages,
            "total_input_tokens": session.total_input_tokens,
            "total_output_tokens": session.total_output_tokens,
            "estimated_context_tokens": session.estimated_context_tokens(),
            "context_near_limit": session.is_context_near_limit(),
            "created_at": session.created_at,
            "last_activity": session.last_activity,
        })).into_response(),
        None => error_response(404, "session_not_found", &format!("Session {id} not found")),
    }
}

// ── Delete Session ─────────────────────────────────────────────────

async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let store = match state.proxy_sessions.as_ref() {
        Some(s) => s,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    if store.delete(&id).await {
        Json(json!({ "deleted": true })).into_response()
    } else {
        error_response(404, "session_not_found", &format!("Session {id} not found"))
    }
}

// ── Send Message ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendMessageRequest {
    /// User message content — string or content blocks array
    content: serde_json::Value,
    /// Optional per-turn max_tokens override
    #[serde(default)]
    max_tokens: Option<u32>,
    /// If true, this is a tool_result response (content should be tool_result blocks)
    #[serde(default)]
    is_tool_result: bool,
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Response {
    let store = match state.proxy_sessions.as_ref() {
        Some(s) => s,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let proxy_state = match state.proxy.as_ref() {
        Some(p) => p,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    // Get session
    let mut session = match store.get(&id).await {
        Some(s) => s,
        None => return error_response(404, "session_not_found", &format!("Session {id} not found")),
    };

    // Check context limit
    if session.is_context_near_limit() {
        return error_response(
            400,
            "context_limit",
            &format!(
                "Session context near limit (~{}K tokens). Create a new session.",
                session.estimated_context_tokens() / 1000
            ),
        );
    }

    // Normalize content: string → content block
    let content = if req.content.is_string() {
        serde_json::json!([{"type": "text", "text": req.content}])
    } else {
        req.content
    };

    // Add message to session
    if req.is_tool_result {
        // Content should be an array of tool_result blocks
        let blocks = if content.is_array() {
            content.as_array().cloned().unwrap_or_default()
        } else {
            vec![content]
        };
        session.add_tool_result(blocks);
    } else {
        session.add_user_message(content);
    }

    // Build API request
    let body = session.build_request(req.max_tokens);
    let extra_betas = session.options.betas.as_deref();

    // Call API
    match proxy::messages_sync(proxy_state, body, extra_betas).await {
        Ok((resp_body, upstream_status)) => {
            if upstream_status == 200 {
                // Record assistant response in session
                session.record_assistant_response(&resp_body);
                store.update(session.clone()).await;

                // Build response with session metadata
                let mut result = resp_body;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("session_id".to_string(), json!(session.id));
                    obj.insert("estimated_context_tokens".to_string(),
                        json!(session.estimated_context_tokens()));
                    obj.insert("context_near_limit".to_string(),
                        json!(session.is_context_near_limit()));

                    // Add rate limit warning if present
                    if let Some(ps) = state.proxy.as_ref() {
                        let rl = ps.rate_limit.read().await;
                        if let Some(warning) = rl.warning_message() {
                            obj.insert("rate_limit_warning".to_string(), json!(warning));
                        }
                    }
                }

                Json(result).into_response()
            } else {
                // Error from API — don't record in session, but save the user message
                // (already added above) so the conversation state is consistent
                // Remove the last message since it wasn't processed
                session.messages.pop();
                store.update(session).await;

                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(resp_body)).into_response()
            }
        }
        Err(e) => {
            // Remove the unprocessed message
            session.messages.pop();
            store.update(session).await;

            let status = StatusCode::from_u16(e.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(json!({
                "error": {
                    "type": e.error_code(),
                    "message": e.to_string(),
                }
            }))).into_response()
        }
    }
}

// ── Send Message (Streaming) ───────────────────────────────────────

/// POST /v1/sessions/:id/msg/stream — SSE streaming within a session.
/// Note: session history is updated AFTER the stream completes (from the
/// final message_stop event). During streaming, the session state is
/// temporarily inconsistent — this is acceptable for development use.
async fn send_message_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Response {
    let store = match state.proxy_sessions.as_ref() {
        Some(s) => s,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };
    let proxy_state = match state.proxy.as_ref() {
        Some(p) => p,
        None => return error_response(501, "proxy_disabled", "Direct API proxy is not enabled"),
    };

    let mut session = match store.get(&id).await {
        Some(s) => s,
        None => return error_response(404, "session_not_found", &format!("Session {id} not found")),
    };

    if session.is_context_near_limit() {
        return error_response(400, "context_limit", "Session context near limit. Create a new session.");
    }

    let content = if req.content.is_string() {
        serde_json::json!([{"type": "text", "text": req.content}])
    } else {
        req.content
    };

    if req.is_tool_result {
        let blocks = if content.is_array() {
            content.as_array().cloned().unwrap_or_default()
        } else {
            vec![content]
        };
        session.add_tool_result(blocks);
    } else {
        session.add_user_message(content);
    }

    let body = session.build_request(req.max_tokens);
    let extra_betas = session.options.betas.clone();

    // Save session with the user message added (will be rolled back on error)
    store.update(session.clone()).await;

    match proxy::messages_stream(proxy_state, body, extra_betas.as_deref()).await {
        Ok((resp, upstream_status)) => {
            if upstream_status != 200 {
                // Rollback: remove unprocessed message
                session.messages.pop();
                store.update(session).await;

                let body = resp.json::<serde_json::Value>().await
                    .unwrap_or_else(|_| json!({"error": {"type": "unknown", "message": "Unknown error"}}));
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(body)).into_response();
            }

            // SSE passthrough — after stream ends, client should call GET /v1/sessions/:id
            // to see the updated session state (or we update asynchronously).
            // For now: passthrough raw SSE, update session from final events in background.
            let byte_stream = resp.bytes_stream();

            // Spawn background task to update session from the stream
            // (We pass through the raw bytes and also accumulate for session update)
            let store_bg = store.clone();
            let session_id = session.id.clone();
            let body = axum::body::Body::from_stream(
                futures::stream::unfold(
                    (byte_stream, String::new()),
                    move |(mut stream, mut accumulated)| {
                        let store_ref = store_bg.clone();
                        let sid = session_id.clone();
                        async move {
                            use futures::StreamExt;
                            match stream.next().await {
                                Some(Ok(bytes)) => {
                                    let chunk = String::from_utf8_lossy(&bytes).to_string();
                                    accumulated.push_str(&chunk);

                                    // Check if stream ended — update session
                                    if chunk.contains("\"type\":\"message_stop\"") {
                                        // Parse accumulated SSE to extract the full message
                                        update_session_from_sse(&store_ref, &sid, &accumulated).await;
                                    }

                                    Some((Ok::<_, std::convert::Infallible>(bytes), (stream, accumulated)))
                                }
                                Some(Err(e)) => {
                                    let err_bytes = axum::body::Bytes::from(format!("data: {{\"error\": \"{e}\"}}\n\n"));
                                    Some((Ok(err_bytes), (stream, accumulated)))
                                }
                                None => None,
                            }
                        }
                    },
                ),
            );

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            session.messages.pop();
            store.update(session).await;

            let status = StatusCode::from_u16(e.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(json!({
                "error": { "type": e.error_code(), "message": e.to_string() }
            }))).into_response()
        }
    }
}

/// Parse accumulated SSE data to extract assistant content and update session.
async fn update_session_from_sse(
    store: &std::sync::Arc<crate::proxy_session::ProxySessionStore>,
    session_id: &str,
    sse_data: &str,
) {
    let mut session = match store.get(session_id).await {
        Some(s) => s,
        None => return,
    };

    // Extract text deltas and tool_use blocks from SSE events
    let mut text_parts = Vec::new();
    let mut content_blocks: Vec<serde_json::Value> = Vec::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    for line in sse_data.lines() {
        let line = line.trim();
        if !line.starts_with("data: ") { continue; }
        let json_str = &line[6..];
        let Ok(event) = serde_json::from_str::<serde_json::Value>(json_str) else { continue };

        match event.get("type").and_then(|t| t.as_str()) {
            Some("content_block_start") => {
                if let Some(block) = event.get("content_block") {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        content_blocks.push(block.clone());
                    }
                }
            }
            Some("content_block_delta") => {
                if let Some(delta) = event.get("delta") {
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(text.to_string());
                            }
                        }
                        Some("input_json_delta") => {
                            // Tool input accumulation — simplified
                        }
                        _ => {}
                    }
                }
            }
            Some("message_start") => {
                if let Some(msg) = event.get("message") {
                    if let Some(usage) = msg.get("usage") {
                        input_tokens += usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        output_tokens += usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                }
            }
            Some("message_delta") => {
                if let Some(usage) = event.get("usage") {
                    output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(output_tokens);
                }
            }
            _ => {}
        }
    }

    // Build assistant message content
    let full_text = text_parts.join("");
    if !full_text.is_empty() {
        content_blocks.insert(0, json!({"type": "text", "text": full_text}));
    }

    if !content_blocks.is_empty() {
        session.messages.push(json!({
            "role": "assistant",
            "content": content_blocks,
        }));
        session.total_input_tokens += input_tokens;
        session.total_output_tokens += output_tokens;
        session.last_activity = crate::proxy_session::epoch_secs_pub();
        store.update(session).await;
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
