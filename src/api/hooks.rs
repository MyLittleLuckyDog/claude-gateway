use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{gateway_error_response, AppState};
use crate::error::GatewayError;
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

#[derive(Deserialize)]
pub struct PermissionResponseRequest {
    pub request_id: String,
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub behavior: Option<String>,
    #[serde(default)]
    pub updated_input: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions/:id/hook_response", post(hook_response))
        .route(
            "/sessions/:id/permission_response",
            post(permission_response),
        )
        .route("/sessions/:id/interrupt", post(interrupt))
}

async fn hook_response(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<HookResponseRequest>,
) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    {
        let current_state = session.state.lock().await;
        match &*current_state {
            SessionState::WaitingForHook {
                request_id,
                deadline,
            } => {
                if *request_id != body.request_id {
                    return gateway_error_response(&GatewayError::InvalidSessionState {
                        expected: format!("waiting for request {}", body.request_id),
                        actual: format!("waiting for request {}", request_id),
                    });
                }
                if std::time::Instant::now() > *deadline {
                    return gateway_error_response(&GatewayError::HookTimeout {
                        hook_id: body.request_id,
                    });
                }
            }
            other => {
                return gateway_error_response(&GatewayError::InvalidSessionState {
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
        Err(e) => {
            return gateway_error_response(&GatewayError::Internal(format!("JSON error: {}", e)))
        }
    };

    if let Some(handle) = session.hook_timeout_handle.lock().await.take() {
        handle.abort();
    }

    match session.stdin_tx.send(json).await {
        Ok(_) => {
            *session.state.lock().await = SessionState::Running;
            (StatusCode::ACCEPTED, Json(json!({}))).into_response()
        }
        Err(_) => gateway_error_response(&GatewayError::Internal("stdin closed".to_string())),
    }
}

async fn interrupt(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    let msg = CliInputMessage::Interrupt;
    let json = match serde_json::to_string(&msg) {
        Ok(j) => j,
        Err(e) => {
            return gateway_error_response(&GatewayError::Internal(format!("JSON error: {}", e)))
        }
    };

    match session.stdin_tx.send(json).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => gateway_error_response(&GatewayError::Internal("stdin closed".to_string())),
    }
}

async fn permission_response(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PermissionResponseRequest>,
) -> Response {
    let session = match state.sessions.get(&id) {
        Ok(s) => s,
        Err(e) => return gateway_error_response(&e),
    };

    let payload = {
        let current_state = session.state.lock().await;
        match &*current_state {
            SessionState::WaitingForPermission {
                request_id,
                original_input,
            } => {
                if *request_id != body.request_id {
                    return gateway_error_response(&GatewayError::InvalidSessionState {
                        expected: format!("waiting for request {}", body.request_id),
                        actual: format!("waiting for request {}", request_id),
                    });
                }
                match build_permission_response_payload(
                    body.response,
                    body.behavior,
                    body.updated_input,
                    body.message,
                    original_input.clone(),
                ) {
                    Ok(v) => v,
                    Err(e) => return gateway_error_response(&e),
                }
            }
            other => {
                return gateway_error_response(&GatewayError::InvalidSessionState {
                    expected: "waiting_for_permission".to_string(),
                    actual: other.to_string(),
                });
            }
        }
    };

    send_permission_response(session, body.request_id, payload).await
}

async fn send_permission_response(
    session: std::sync::Arc<crate::session::Session>,
    request_id: String,
    payload: Value,
) -> Response {
    let response = ControlResponseOut::success(request_id, payload);
    let json = match serde_json::to_string(&response) {
        Ok(j) => j,
        Err(e) => {
            return gateway_error_response(&GatewayError::Internal(format!("JSON error: {}", e)))
        }
    };

    match session.stdin_tx.send(json).await {
        Ok(_) => {
            *session.state.lock().await = SessionState::Running;
            (StatusCode::ACCEPTED, Json(json!({}))).into_response()
        }
        Err(_) => gateway_error_response(&GatewayError::Internal("stdin closed".to_string())),
    }
}

fn build_permission_response_payload(
    response: Option<Value>,
    behavior: Option<String>,
    updated_input: Option<Value>,
    message: Option<String>,
    original_input: Value,
) -> Result<Value, GatewayError> {
    if let Some(v) = response {
        return Ok(v);
    }

    let behavior = behavior.unwrap_or_else(|| "deny".to_string());
    match behavior.as_str() {
        "allow" => {
            let mut obj = serde_json::Map::new();
            obj.insert("behavior".to_string(), Value::String("allow".to_string()));
            obj.insert(
                "updatedInput".to_string(),
                updated_input.unwrap_or(original_input),
            );
            Ok(Value::Object(obj))
        }
        "deny" => {
            let mut obj = serde_json::Map::new();
            obj.insert("behavior".to_string(), Value::String("deny".to_string()));
            if let Some(v) = message {
                obj.insert("message".to_string(), Value::String(v));
            }
            Ok(Value::Object(obj))
        }
        other => Err(GatewayError::Internal(format!(
            "invalid permission behavior: {}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::build_permission_response_payload;

    #[test]
    fn builds_allow_permission_payload() {
        let payload = build_permission_response_payload(
            None,
            Some("allow".to_string()),
            Some(json!({"command": "git diff"})),
            None,
            json!({"command": "git status"}),
        )
        .unwrap();
        assert_eq!(payload["behavior"], "allow");
        assert_eq!(payload["updatedInput"]["command"], "git diff");
    }

    #[test]
    fn builds_deny_permission_payload() {
        let payload = build_permission_response_payload(
            None,
            Some("deny".to_string()),
            None,
            Some("blocked by policy".to_string()),
            json!({"command": "git status"}),
        )
        .unwrap();
        assert_eq!(payload["behavior"], "deny");
        assert_eq!(payload["message"], "blocked by policy");
    }

    #[test]
    fn allow_permission_payload_falls_back_to_original_input() {
        let payload = build_permission_response_payload(
            None,
            Some("allow".to_string()),
            None,
            None,
            json!({"command": "git status"}),
        )
        .unwrap();
        assert_eq!(payload["behavior"], "allow");
        assert_eq!(payload["updatedInput"]["command"], "git status");
    }
}
