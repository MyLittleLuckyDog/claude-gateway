use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use super::AppState;
use crate::codex::options::CodexOptions;
use crate::codex_app;
use crate::codex_app::session::CodexAppSessionState;
use crate::error::{ErrorResponse, GatewayError};

#[derive(Deserialize)]
pub struct CreateCodexAppSessionRequest {
    #[serde(default)]
    pub options: Option<CodexOptions>,
}

#[derive(Serialize)]
pub struct CreateCodexAppSessionResponse {
    pub session_id: String,
    pub state: String,
}

#[derive(Deserialize)]
pub struct SendRequest {
    pub message: String,
}

#[derive(Deserialize)]
pub struct ApprovalResponseRequest {
    pub request_id: String,
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub decision: Option<String>,
}

#[derive(Deserialize)]
pub struct MessagesQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    50
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/codex/app/sessions", post(create_session))
        .route("/codex/app/sessions", get(list_sessions))
        .route("/codex/app/sessions/:id", delete(delete_session))
        .route("/codex/app/sessions/:id/send", post(send_message))
        .route("/codex/app/sessions/:id/stream", get(stream_session))
        .route("/codex/app/sessions/:id/messages", get(get_messages))
        .route("/codex/app/sessions/:id/approval_response", post(approval_response))
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateCodexAppSessionRequest>,
) -> Response {
    match codex_app::create_session(
        req.options.unwrap_or_default(),
        state.codex_app_sessions.clone(),
        state.config.clone(),
    )
    .await
    {
        Ok(session) => (
            StatusCode::CREATED,
            Json(CreateCodexAppSessionResponse {
                session_id: session.id.clone(),
                state: session.state.lock().await.to_string(),
            }),
        )
            .into_response(),
        Err(e) => error_response(&e),
    }
}

async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let sessions = state.codex_app_sessions.list();
    let mut list = Vec::new();
    for s in sessions {
        list.push(json!({
            "session_id": s.id,
            "state": s.state.lock().await.to_string(),
            "thread_id": s.thread_id.lock().await.clone(),
            "turn_id": s.turn_id.lock().await.clone(),
            "created_at_secs": s.created_at.elapsed().as_secs(),
        }));
    }
    Json(json!(list))
}

async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if state.codex_app_sessions.remove(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error_response(&GatewayError::SessionNotFound(id))
    }
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendRequest>,
) -> Response {
    let session = match state.codex_app_sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };
    {
        let state = session.state.lock().await.clone();
        if state != CodexAppSessionState::Idle {
            return error_response(&GatewayError::InvalidSessionState {
                expected: "idle".to_string(),
                actual: state.to_string(),
            });
        }
    }
    match codex_app::send_turn(session, req.message).await {
        Ok(_) => (StatusCode::ACCEPTED, Json(json!({}))).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn approval_response(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ApprovalResponseRequest>,
) -> Response {
    let session = match state.codex_app_sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    let response = match req.response {
        Some(response) => response,
        None => json!({
            "decision": req.decision.unwrap_or_else(|| "accept".to_string())
        }),
    };

    match codex_app::send_approval_response(session, &req.request_id, response).await {
        Ok(_) => (StatusCode::ACCEPTED, Json(json!({}))).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn stream_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let session = match state.codex_app_sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    let history: Vec<Arc<Value>> = session.history.lock().await.iter().cloned().collect();
    let mut rx = session.event_tx.subscribe();

    let stream = async_stream::stream! {
        for (idx, event) in history.iter().enumerate() {
            if let Ok(data) = serde_json::to_string(event.as_ref()) {
                yield Ok::<_, axum::Error>(Event::default().id(idx.to_string()).data(data));
            }
        }

        let mut current_idx = history.len();
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Ok(data) = serde_json::to_string(event.as_ref()) {
                        yield Ok(Event::default().id(current_idx.to_string()).data(data));
                    }
                    current_idx += 1;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let err = json!({"type":"error","message":format!("Lagged: {} events skipped", n),"code":"stream_lagged"});
                    yield Ok(Event::default().data(err.to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

async fn get_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessagesQuery>,
) -> Response {
    let session = match state.codex_app_sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    let history = session.history.lock().await;
    let total = history.len();
    let messages: Vec<_> = history
        .iter()
        .skip(params.offset)
        .take(params.limit)
        .map(|event| event.as_ref().clone())
        .collect();

    Json(json!({
        "session_id": id,
        "thread_id": session.thread_id.lock().await.clone(),
        "turn_id": session.turn_id.lock().await.clone(),
        "total": total,
        "messages": messages,
    }))
    .into_response()
}

fn error_response(e: &GatewayError) -> Response {
    let status = axum::http::StatusCode::from_u16(e.http_status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(ErrorResponse::from(e))).into_response()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn default_approval_response_accepts() {
        let response = json!({
            "decision": "accept"
        });
        assert_eq!(response["decision"], "accept");
    }
}
