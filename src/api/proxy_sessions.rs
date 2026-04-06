//! HTTP handlers for proxy-mode multi-turn sessions.
//!
//! POST   /v1/sessions          — create session
//! GET    /v1/sessions          — list sessions (when proxy enabled, merged via query param)
//! POST   /v1/sessions/:id/msg — send message → get response
//! GET    /v1/sessions/:id     — get session state
//! DELETE /v1/sessions/:id     — delete session

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::api::AppState;
use crate::proxy;
use crate::proxy_session::SessionOptions;
use crate::sse::{SseParser, StreamAccumulator};

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

    match store.get_snapshot(&id).await {
        Some(session) => Json(json!({
            "id": session.id,
            "model": session.model,
            "system": session.system,
            "messages": session.messages,
            "total_input_tokens": session.total_input_tokens,
            "total_output_tokens": session.total_output_tokens,
            "last_input_tokens": session.last_input_tokens,
            "last_output_tokens": session.last_output_tokens,
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

    // Normalize content: string → content block
    let content = if req.content.is_string() {
        serde_json::json!([{"type": "text", "text": req.content}])
    } else {
        req.content
    };

    // Phase 1: Add message and build request while holding the session lock.
    // Returns (body, betas, session_id, message_index) — the index is used
    // for safe rollback if the API call fails.
    #[allow(clippy::question_mark)]
    let prepared = store.with_session(&id, |session| {
        let max_tokens = req.max_tokens
            .or(session.options.max_tokens)
            .unwrap_or_else(|| crate::models::default_max_tokens(&session.model));

        if let Err(msg) = session.preflight_check(max_tokens) {
            return Err(msg);
        }

        if req.is_tool_result {
            let blocks = if content.is_array() {
                content.as_array().cloned().unwrap_or_default()
            } else {
                vec![content.clone()]
            };
            session.add_tool_result(blocks);
        } else {
            session.add_user_message(content.clone());
        }

        let msg_index = session.messages.len() - 1;
        let body = session.build_request(req.max_tokens);
        let betas = session.options.betas.clone();
        let upstream_sid = session.id.clone();
        Ok((body, betas, upstream_sid, msg_index))
    }).await;

    let (body, extra_betas, upstream_session_id, msg_index) = match prepared {
        None => return error_response(404, "session_not_found", &format!("Session {id} not found")),
        Some(Err(msg)) => return error_response(400, "context_limit", &msg),
        Some(Ok(tuple)) => tuple,
    };

    // Phase 2: Call API (lock released)
    match proxy::messages_sync(proxy_state, body, extra_betas.as_deref(), &upstream_session_id).await {
        Ok((resp_body, upstream_status)) => {
            if upstream_status == 200 {
                // Phase 3: Record response while holding the lock
                let session_meta = store.with_session(&id, |session| {
                    session.record_assistant_response(&resp_body);
                    (
                        session.id.clone(),
                        session.estimated_context_tokens(),
                        session.is_context_near_limit(),
                    )
                }).await;

                let (sid, ctx_tokens, near_limit) = match session_meta {
                    Some(meta) => meta,
                    None => {
                        tracing::warn!(
                            "Session {} was deleted during API call; \
                             assistant response lost",
                            id
                        );
                        (id.clone(), 0, false)
                    }
                };

                let mut result = resp_body;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("session_id".to_string(), json!(sid));
                    obj.insert("estimated_context_tokens".to_string(), json!(ctx_tokens));
                    obj.insert("context_near_limit".to_string(), json!(near_limit));

                    if let Some(ps) = state.proxy.as_ref() {
                        let rl = ps.rate_limit.read().await;
                        if let Some(warning) = rl.warning_message() {
                            obj.insert("rate_limit_warning".to_string(), json!(warning));
                        }
                    }
                }

                Json(result).into_response()
            } else {
                // Rollback: remove the message we added
                let _ = store.with_session(&id, |session| {
                    session.rollback_message_at(msg_index, "user");
                }).await;

                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                (status, Json(resp_body)).into_response()
            }
        }
        Err(e) => {
            let _ = store.with_session(&id, |session| {
                session.rollback_message_at(msg_index, "user");
            }).await;

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
#[allow(clippy::question_mark)]
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

    let content = if req.content.is_string() {
        serde_json::json!([{"type": "text", "text": req.content}])
    } else {
        req.content
    };

    // Phase 1: Add message and build request under lock
    let prepared = store.with_session(&id, |session| {
        let max_tokens = req.max_tokens
            .or(session.options.max_tokens)
            .unwrap_or_else(|| crate::models::default_max_tokens(&session.model));

        if let Err(msg) = session.preflight_check(max_tokens) {
            return Err(msg);
        }

        if req.is_tool_result {
            let blocks = if content.is_array() {
                content.as_array().cloned().unwrap_or_default()
            } else {
                vec![content.clone()]
            };
            session.add_tool_result(blocks);
        } else {
            session.add_user_message(content.clone());
        }

        let msg_index = session.messages.len() - 1;
        let body = session.build_request(req.max_tokens);
        let betas = session.options.betas.clone();
        let upstream_sid = session.id.clone();
        Ok((body, betas, upstream_sid, msg_index))
    }).await;

    let (body, extra_betas, upstream_session_id, msg_index) = match prepared {
        None => return error_response(404, "session_not_found", &format!("Session {id} not found")),
        Some(Err(msg)) => return error_response(400, "context_limit", &msg),
        Some(Ok(tuple)) => tuple,
    };

    // Phase 2: Call API (lock released)
    match proxy::messages_stream(proxy_state, body, extra_betas.as_deref(), &upstream_session_id).await {
        Ok((resp, upstream_status)) => {
            if upstream_status != 200 {
                // Rollback
                let _ = store.with_session(&id, |session| {
                    session.rollback_message_at(msg_index, "user");
                }).await;

                let body = resp.json::<serde_json::Value>().await
                    .unwrap_or_else(|_| json!({"error": {"type": "unknown", "message": "Unknown error"}}));
                let status = StatusCode::from_u16(upstream_status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return (status, Json(body)).into_response();
            }

            // SSE passthrough with inline parsing: raw bytes are forwarded
            // to the client unchanged, while SseParser + StreamAccumulator
            // extract the assistant message and usage for session update.
            let byte_stream = resp.bytes_stream();

            let parser = Arc::new(Mutex::new(SseParser::new()));
            let accumulator = Arc::new(Mutex::new(StreamAccumulator::new()));
            let store_bg = store.clone();
            let session_id = upstream_session_id.clone();
            let parser_bg = parser.clone();
            let acc_bg = accumulator.clone();

            // Track whether the stream completed successfully so we can
            // roll back the user message on abort/error.
            let stream_completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_flag = stream_completed.clone();

            let body = axum::body::Body::from_stream(
                futures::stream::unfold(
                    byte_stream,
                    move |mut stream| {
                        let store_ref = store_bg.clone();
                        let sid = session_id.clone();
                        let parser_ref = parser_bg.clone();
                        let acc_ref = acc_bg.clone();
                        let completed = completed_flag.clone();
                        async move {
                            use futures::StreamExt;
                            match stream.next().await {
                                Some(Ok(bytes)) => {
                                    let events = {
                                        let mut p = parser_ref.lock().await;
                                        p.push(&bytes)
                                    };

                                    let is_complete = {
                                        let mut a = acc_ref.lock().await;
                                        for event in &events {
                                            a.process_event(event);
                                        }
                                        a.is_complete()
                                    };

                                    if is_complete {
                                        update_session_from_accumulator(
                                            &store_ref, &sid, &acc_ref,
                                        ).await;
                                        completed.store(true, std::sync::atomic::Ordering::Release);
                                    }

                                    Some((Ok::<_, std::convert::Infallible>(bytes), stream))
                                }
                                Some(Err(e)) => {
                                    tracing::warn!("SSE stream error, rolling back session {}: {e}", sid);
                                    let _ = store_ref.with_session(&sid, |session| {
                                        session.rollback_message_at(msg_index, "user");
                                    }).await;
                                    completed.store(true, std::sync::atomic::Ordering::Release);
                                    None
                                }
                                None => {
                                    let trailing = {
                                        let mut p = parser_ref.lock().await;
                                        p.finish()
                                    };
                                    if !trailing.is_empty() {
                                        let mut a = acc_ref.lock().await;
                                        for event in &trailing {
                                            a.process_event(event);
                                        }
                                        if a.is_complete() {
                                            drop(a);
                                            update_session_from_accumulator(
                                                &store_ref, &sid, &acc_ref,
                                            ).await;
                                            completed.store(true, std::sync::atomic::Ordering::Release);
                                        }
                                    }

                                    if !completed.load(std::sync::atomic::Ordering::Acquire) {
                                        tracing::warn!(
                                            "SSE stream ended without message_stop, \
                                             rolling back session {}",
                                            sid
                                        );
                                        let _ = store_ref.with_session(&sid, |session| {
                                            session.rollback_message_at(msg_index, "user");
                                        }).await;
                                    }

                                    None
                                }
                            }
                        }
                    },
                ),
            );

            // Background safety net for client disconnect (stream drop).
            // When the client disconnects, the Body is dropped and the unfold
            // future is cancelled — the None branch never runs.
            let rollback_store = store.clone();
            let rollback_sid = upstream_session_id.clone();
            let rollback_completed = stream_completed.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                if !rollback_completed.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::warn!(
                        "SSE stream for session {} timed out without completion, rolling back",
                        rollback_sid
                    );
                    let _ = rollback_store.with_session(&rollback_sid, |session| {
                        session.rollback_message_at(msg_index, "user");
                    }).await;
                }
            });

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            let _ = store.with_session(&id, |session| {
                session.rollback_message_at(msg_index, "user");
            }).await;

            let status = StatusCode::from_u16(e.http_status())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (status, Json(json!({
                "error": { "type": e.error_code(), "message": e.to_string() }
            }))).into_response()
        }
    }
}

/// Update session state from a completed StreamAccumulator.
async fn update_session_from_accumulator(
    store: &std::sync::Arc<crate::proxy_session::ProxySessionStore>,
    session_id: &str,
    accumulator: &Arc<Mutex<StreamAccumulator>>,
) {
    // Swap out the accumulator to take ownership
    let acc = {
        let mut guard = accumulator.lock().await;
        std::mem::replace(&mut *guard, StreamAccumulator::new())
    };

    let input_tokens = acc.input_tokens;
    let output_tokens = acc.output_tokens;
    let content_blocks = acc.into_content_blocks();

    if let Some(blocks) = content_blocks {
        let _ = store.with_session(session_id, |session| {
            session.messages.push(json!({
                "role": "assistant",
                "content": blocks,
            }));
            session.total_input_tokens += input_tokens;
            session.total_output_tokens += output_tokens;
            session.last_input_tokens = input_tokens;
            session.last_output_tokens = output_tokens;
            session.last_activity = crate::proxy_session::epoch_secs();
        }).await;
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
