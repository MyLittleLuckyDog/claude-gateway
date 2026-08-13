use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use super::{gateway_error_response, AppState};
use crate::client;
use crate::core::events::sse_replay_then_follow;
use crate::error::GatewayError;
use crate::messages::cli_input::{CliInputMessage, CliUserInput, ImageSource, InputContent};
use crate::messages::Message;
use crate::options::ClaudeAgentOptions;
use crate::session::SessionState;

#[derive(Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub options: Option<ClaudeAgentOptions>,
}

#[derive(Serialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub state: String,
}

#[derive(Deserialize)]
pub struct SendRequest {
    pub message: String,
    #[serde(default)]
    pub image_base64: Option<String>,
    #[serde(default)]
    pub image_media_type: Option<String>,
}

#[derive(Deserialize)]
pub struct MessagesQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub include_system: bool,
}

fn default_limit() -> usize {
    50
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions", post(create_session))
        .route("/sessions", get(list_sessions))
        .route("/sessions/:id", delete(delete_session))
        .route("/sessions/:id/send", post(send_message))
        .route("/sessions/:id/stream", get(stream_session))
        .route("/sessions/:id/messages", get(get_messages))
        .route("/sessions/:id/fork", post(fork_session))
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    let options = req.options.unwrap_or_default();
    match client::create_session(options, state.sessions.clone(), state.config.clone()).await {
        Ok(session) => {
            let resp = CreateSessionResponse {
                session_id: session.id.clone(),
                state: session.state.lock().await.to_string(),
            };
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => gateway_error_response(&e),
    }
}

async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let sessions = state.sessions.list();
    let mut list = Vec::new();
    for s in sessions {
        let st = s.state.lock().await.to_string();
        list.push(serde_json::json!({
            "session_id": s.id,
            "state": st,
            "created_at_secs": s.created_at.elapsed().as_secs(),
        }));
    }
    Json(serde_json::json!(list))
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.sessions.remove(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        gateway_error_response(&GatewayError::SessionNotFound(id))
    }
}

async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendRequest>,
) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    // Atomic check-and-set: acquire lock once, verify state, transition to Running
    let previous_state = {
        let mut current_state = session.state.lock().await;
        match take_sendable_state(&current_state) {
            Ok(previous) => {
                *current_state = SessionState::Running;
                previous
            }
            Err(e) => return gateway_error_response(&e),
        }
    };

    // Build message
    let mut content = vec![InputContent::Text { text: req.message }];
    if let (Some(data), Some(media_type)) = (req.image_base64, req.image_media_type) {
        content.push(InputContent::Image {
            source: ImageSource {
                source_type: "base64".to_string(),
                media_type,
                data,
            },
        });
    }

    let msg = CliInputMessage::User {
        message: CliUserInput {
            role: "user".to_string(),
            content,
        },
    };

    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => {
            return gateway_error_response(&GatewayError::Internal(format!("JSON error: {}", e)))
        }
    };

    // Lock-free activity timestamp
    session.last_activity_ms.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );

    match session.stdin_tx.send(json).await {
        Ok(_) => (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response(),
        Err(_) => {
            *session.state.lock().await = previous_state;
            gateway_error_response(&GatewayError::Internal("stdin closed".to_string()))
        }
    }
}

fn take_sendable_state(current_state: &SessionState) -> Result<SessionState, GatewayError> {
    match current_state {
        SessionState::Initializing | SessionState::Idle => Ok(current_state.clone()),
        other => Err(GatewayError::InvalidSessionState {
            expected: "initializing or idle".to_string(),
            actual: other.to_string(),
        }),
    }
}

async fn stream_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    // Clone Arc refs (cheap) not full Messages
    let history: Vec<Arc<Message>> = session.history.lock().await.iter().cloned().collect();
    let rx = session.event_tx.subscribe();

    sse_replay_then_follow(history, rx).into_response()
}

async fn get_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessagesQuery>,
) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    let history = session.history.lock().await;
    let filtered: Vec<&Message> = history
        .iter()
        .map(|m| m.as_ref())
        .filter(|m| {
            if !params.include_system {
                !matches!(m, Message::System { .. })
            } else {
                true
            }
        })
        .collect();

    let total = filtered.len();
    let messages: Vec<_> = filtered
        .into_iter()
        .skip(params.offset)
        .take(params.limit)
        .collect();

    Json(serde_json::json!({
        "session_id": id,
        "total": total,
        "messages": messages,
    }))
    .into_response()
}

async fn fork_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    // Get the CLI session ID to use with --resume
    let cli_session_id = session.cli_session_id.lock().await.clone();
    let cli_session_id = match cli_session_id {
        Some(id) => id,
        None => {
            return gateway_error_response(&GatewayError::InvalidSessionState {
                expected: "session with CLI session ID".to_string(),
                actual: "no CLI session ID (not yet initialized)".to_string(),
            });
        }
    };

    // Create new session with --resume pointing to the original CLI session
    let mut new_options = session.options.clone();
    new_options.resume = Some(cli_session_id);
    new_options.fork_session = Some("true".to_string());

    match client::create_session(new_options, state.sessions.clone(), state.config.clone()).await {
        Ok(new_session) => {
            let resp = CreateSessionResponse {
                session_id: new_session.id.clone(),
                state: new_session.state.lock().await.to_string(),
            };
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) => gateway_error_response(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::take_sendable_state;
    use crate::session::SessionState;

    #[test]
    fn sendable_states_are_preserved_for_rollback() {
        assert_eq!(
            take_sendable_state(&SessionState::Idle).unwrap(),
            SessionState::Idle
        );
        assert_eq!(
            take_sendable_state(&SessionState::Initializing).unwrap(),
            SessionState::Initializing
        );
    }

    #[test]
    fn waiting_states_are_rejected_for_send() {
        let err = take_sendable_state(&SessionState::WaitingForPermission {
            request_id: "req_1".to_string(),
            original_input: serde_json::json!({"command": "git status"}),
        })
        .unwrap_err();
        assert!(err.to_string().contains("wrong state"));
    }
}
