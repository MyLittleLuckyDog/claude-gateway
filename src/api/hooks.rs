use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;

use super::AppState;
use crate::error::{ErrorResponse, GatewayError};
use crate::messages::cli_input::{CliInputMessage, HookDecision};
use crate::session::SessionState;

#[derive(Deserialize)]
pub struct HookResponseRequest {
    pub hook_id: String,
    pub decision: HookDecision,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub updated_input: Option<Value>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions/:id/hook_response", post(hook_response))
        .route("/sessions/:id/interrupt", post(interrupt))
}

async fn hook_response(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<HookResponseRequest>,
) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    // Validate state
    {
        let current_state = session.state.lock().await;
        match &*current_state {
            SessionState::WaitingForHook { hook_id, deadline } => {
                if *hook_id != body.hook_id {
                    return error_response(&GatewayError::InvalidSessionState {
                        expected: format!("waiting for hook {}", body.hook_id),
                        actual: format!("waiting for hook {}", hook_id),
                    });
                }
                if std::time::Instant::now() > *deadline {
                    return error_response(&GatewayError::HookTimeout {
                        hook_id: body.hook_id,
                    });
                }
            }
            other => {
                return error_response(&GatewayError::InvalidSessionState {
                    expected: "waiting_for_hook".to_string(),
                    actual: other.to_string(),
                });
            }
        }
    }

    // Build response message
    let msg = CliInputMessage::HookResponse {
        hook_id: body.hook_id,
        decision: body.decision,
        reason: body.reason,
        updated_input: body.updated_input,
        suppress_output: None,
    };

    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => return error_response(&GatewayError::Internal(format!("JSON error: {}", e))),
    };

    // Cancel pending hook timeout task before transitioning state
    if let Some(handle) = session.hook_timeout_handle.lock().await.take() {
        handle.abort();
    }

    // Send to CLI stdin
    match session.stdin_tx.send(json).await {
        Ok(_) => {
            *session.state.lock().await = SessionState::Running;
            (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response()
        }
        Err(_) => error_response(&GatewayError::Internal("stdin closed".to_string())),
    }
}

async fn interrupt(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return error_response(&e),
    };

    let msg = CliInputMessage::Interrupt;
    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => return error_response(&GatewayError::Internal(format!("JSON error: {}", e))),
    };

    match session.stdin_tx.send(json).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error_response(&GatewayError::Internal("stdin closed".to_string())),
    }
}

fn error_response(e: &GatewayError) -> Response {
    let status = axum::http::StatusCode::from_u16(e.http_status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(ErrorResponse::from(e))).into_response()
}
