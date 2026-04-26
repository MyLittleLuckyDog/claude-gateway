use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

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
use serde_json::Value;
use tokio::sync::{broadcast, Mutex};

use super::AppState;
use crate::codex;
use crate::codex::messages::CodexEvent;
use crate::codex::options::CodexOptions;
use crate::codex::session::{CodexSession, CodexSessionState};
use crate::error::{ErrorResponse, GatewayError};

#[derive(Deserialize)]
pub struct CodexQueryRequest {
    pub prompt: String,
    #[serde(default)]
    pub options: Option<CodexOptions>,
}

#[derive(Deserialize)]
pub struct CreateCodexSessionRequest {
    #[serde(default)]
    pub options: Option<CodexOptions>,
}

#[derive(Serialize)]
pub struct CreateCodexSessionResponse {
    pub session_id: String,
    pub state: String,
}

#[derive(Deserialize)]
pub struct SendCodexRequest {
    pub message: String,
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
        .route("/codex/query", post(query_handler))
        .route("/codex/query/stream", post(query_stream_handler))
        .route("/codex/sessions", post(create_session))
        .route("/codex/sessions", get(list_sessions))
        .route("/codex/sessions/:id", delete(delete_session))
        .route("/codex/sessions/:id/send", post(send_message))
        .route("/codex/sessions/:id/stream", get(stream_session))
        .route("/codex/sessions/:id/messages", get(get_messages))
}

async fn query_handler(
    State(state): State<AppState>,
    Json(req): Json<CodexQueryRequest>,
) -> Response {
    let options = req.options.unwrap_or_default();
    match codex::query(&req.prompt, options, &state.config).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => error_response(&e),
    }
}

async fn query_stream_handler(
    State(state): State<AppState>,
    Json(req): Json<CodexQueryRequest>,
) -> Response {
    let options = req.options.unwrap_or_default();
    match codex::query_stream(&req.prompt, options, &state.config).await {
        Ok(mut rx) => {
            let stream = async_stream::stream! {
                while let Some(event) = rx.recv().await {
                    match serde_json::to_string(&event) {
                        Ok(data) => yield Ok::<_, axum::Error>(Event::default().data(data)),
                        Err(_) => continue,
                    }
                }
                yield Ok(Event::default().data("[DONE]"));
            };
            Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
        }
        Err(e) => error_response(&e),
    }
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateCodexSessionRequest>,
) -> Response {
    let session_id = uuid::Uuid::new_v4().to_string();
    let (event_tx, _) = broadcast::channel::<Arc<CodexEvent>>(1024);
    let session = Arc::new(CodexSession {
        id: session_id.clone(),
        thread_id: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(CodexSessionState::Idle)),
        created_at: std::time::Instant::now(),
        last_activity_ms: AtomicU64::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        ),
        options: req.options.unwrap_or_default(),
        event_tx,
        history: Arc::new(Mutex::new(VecDeque::new())),
    });

    match state.codex_sessions.insert(session.clone()) {
        Ok(_) => (
            StatusCode::CREATED,
            Json(CreateCodexSessionResponse {
                session_id,
                state: session.state.lock().await.to_string(),
            }),
        )
            .into_response(),
        Err(e) => error_response(&e),
    }
}

async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let sessions = state.codex_sessions.list();
    let mut list = Vec::new();
    for s in sessions {
        let state_name = s.state.lock().await.to_string();
        let thread_id = s.thread_id.lock().await.clone();
        list.push(serde_json::json!({
            "session_id": s.id,
            "state": state_name,
            "thread_id": thread_id,
            "created_at_secs": s.created_at.elapsed().as_secs(),
        }));
    }
    Json(serde_json::json!(list))
}

async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if state.codex_sessions.remove(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        error_response(&GatewayError::SessionNotFound(id))
    }
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendCodexRequest>,
) -> Response {
    let session = match state.codex_sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    {
        let mut current_state = session.state.lock().await;
        if *current_state != CodexSessionState::Idle {
            return error_response(&GatewayError::InvalidSessionState {
                expected: "idle".to_string(),
                actual: current_state.to_string(),
            });
        }
        *current_state = CodexSessionState::Running;
    }

    session.last_activity_ms.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );

    let session_clone = session.clone();
    let config = state.config.clone();
    tokio::spawn(async move {
        if let Err(e) = codex::run_session_turn(session_clone.clone(), req.message, config).await {
            tracing::error!("codex session {} turn error: {}", session_clone.id, e);
        }
    });

    (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response()
}

async fn stream_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let session = match state.codex_sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    let history: Vec<Arc<CodexEvent>> = session.history.lock().await.iter().cloned().collect();
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
                    let err = serde_json::json!({"type":"error","message":format!("Lagged: {} events skipped", n),"code":"stream_lagged"});
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
    let session = match state.codex_sessions.get(&id) {
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

    Json(serde_json::json!({
        "session_id": id,
        "thread_id": session.thread_id.lock().await.clone(),
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
