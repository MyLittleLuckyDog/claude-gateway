use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppState;
use crate::error::{ErrorResponse, GatewayError};
use crate::messages::cli_control::ControlResponseOut;
use crate::messages::cli_input::CliInputMessage;
use crate::session::SessionState;

/// Client-driven hook response body.
///
/// For the common deny/allow cases clients pass `{"decision": "block", "reason": "..."}`
/// or `{"decision": "approve"}`. Any additional fields are preserved in the
/// `response` object forwarded to the CLI.
#[derive(Deserialize)]
pub struct HookResponseRequest {
    pub request_id: String,
    /// Raw control_response `response` payload (e.g. `{"decision":"block","reason":"..."}`).
    /// If omitted we synthesize one from `decision` + `reason`.
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub decision: Option<String>,
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

    {
        let current_state = session.state.lock().await;
        match &*current_state {
            SessionState::WaitingForHook { request_id, deadline } => {
                if *request_id != body.request_id {
                    return error_response(&GatewayError::InvalidSessionState {
                        expected: format!("waiting for request {}", body.request_id),
                        actual: format!("waiting for request {}", request_id),
                    });
                }
                if std::time::Instant::now() > *deadline {
                    return error_response(&GatewayError::HookTimeout {
                        hook_id: body.request_id,
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

    // Build the inner response payload. Explicit `response` wins; otherwise we
    // compose `{decision, reason?, updatedInput?}` — the keys the CLI expects.
    let payload = match body.response {
        Some(v) => v,
        None => {
            let mut obj = serde_json::Map::new();
            if let Some(d) = body.decision {
                obj.insert("decision".to_string(), Value::String(d));
            }
            if let Some(r) = body.reason {
                obj.insert("reason".to_string(), Value::String(r));
            }
            if let Some(u) = body.updated_input {
                // CLI-side field is camelCase (updatedInput).
                obj.insert("updatedInput".to_string(), u);
            }
            Value::Object(obj)
        }
    };

    let response = ControlResponseOut::success(body.request_id, payload);
    let json = match serde_json::to_string(&response) {
        Ok(j) => j,
        Err(e) => return error_response(&GatewayError::Internal(format!("JSON error: {}", e))),
    };

    if let Some(handle) = session.hook_timeout_handle.lock().await.take() {
        handle.abort();
    }

    match session.stdin_tx.send(json).await {
        Ok(_) => {
            *session.state.lock().await = SessionState::Running;
            (StatusCode::ACCEPTED, Json(json!({}))).into_response()
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
