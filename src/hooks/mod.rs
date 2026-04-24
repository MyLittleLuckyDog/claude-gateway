pub mod server_rules;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::messages::Message;
use crate::messages::cli_control::{ControlResponseOut, HookCallbackInput, HookCallbackRequest};
use crate::options::ClaudeAgentOptions;
use crate::session::{Session, SessionState, MAX_HISTORY_SIZE};

use server_rules::{evaluate_hook_rules, ResolvedDecision};

/// Build the `hooks` object for the `initialize` control_request and the
/// per-session callback_id → rule map. Returns `None` when no rules configured.
///
/// Shape produced (matches SDK):
/// ```text
/// {
///   "PreToolUse": [
///     {"matcher": "Bash", "hookCallbackIds": ["hook_0"]},
///     {"matcher": "Read", "hookCallbackIds": ["hook_1"]}
///   ]
/// }
/// ```
pub fn build_initialize_hooks(options: &ClaudeAgentOptions) -> Option<(Value, HashMap<String, usize>)> {
    let rules = options.hook_rules.as_ref()?;
    if rules.is_empty() {
        return None;
    }
    let mut by_event: HashMap<String, Vec<Value>> = HashMap::new();
    let mut callback_map: HashMap<String, usize> = HashMap::new();
    for (idx, rule) in rules.iter().enumerate() {
        let callback_id = format!("hook_{}", idx);
        callback_map.insert(callback_id.clone(), idx);
        let matcher = rule.tool_pattern.clone().unwrap_or_else(|| "*".to_string());
        by_event.entry(rule.event.clone()).or_default().push(json!({
            "matcher": matcher,
            "hookCallbackIds": [callback_id],
        }));
    }
    Some((json!(by_event), callback_map))
}

/// Decision outcome of `try_auto_resolve_hook` — lets the caller drive the
/// session state machine without ever building CLI JSON itself.
pub enum AutoResolveOutcome {
    /// Respond to the CLI immediately with this control_response JSON.
    Respond(String),
    /// No rule matched — hand off to the streaming client via WaitingForHook.
    DeferToClient,
}

/// Evaluate server-side rules for an inbound hook_callback control_request.
///
/// * `options` — session options carrying `hook_rules`
/// * `request_id` — outer control_request.request_id (needed to form the response)
/// * `callback` — parsed inner `hook_callback` payload
pub fn try_auto_resolve_hook(
    options: &ClaudeAgentOptions,
    request_id: &str,
    callback: &HookCallbackRequest,
) -> AutoResolveOutcome {
    let input = HookCallbackInput::from_value(&callback.input);
    match evaluate_hook_rules(options, &input) {
        Some(ResolvedDecision::Approve) => {
            let payload = json!({ "decision": "approve" });
            emit(request_id.to_string(), payload)
        }
        Some(ResolvedDecision::Block { reason }) => {
            let mut payload = json!({ "decision": "block" });
            if let Some(r) = reason {
                payload["reason"] = Value::String(r);
            }
            emit(request_id.to_string(), payload)
        }
        Some(ResolvedDecision::Defer) | None => AutoResolveOutcome::DeferToClient,
    }
}

fn emit(request_id: String, payload: Value) -> AutoResolveOutcome {
    let response = ControlResponseOut::success(request_id, payload);
    match serde_json::to_string(&response) {
        Ok(s) => AutoResolveOutcome::Respond(s),
        Err(e) => {
            tracing::error!("Failed to encode control_response: {}", e);
            AutoResolveOutcome::DeferToClient
        }
    }
}

/// Start a watchdog that auto-approves after 30s when waiting on the client.
/// Uses `request_id` (not a legacy `hook_id`) to identify the pending callback.
pub fn spawn_hook_timeout(session: Arc<Session>, request_id: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let mut state = session.state.lock().await;
        if let SessionState::WaitingForHook {
            request_id: ref waiting_id,
            ..
        } = *state
        {
            if *waiting_id == request_id {
                tracing::warn!("Hook request {} timed out, auto-approving", request_id);
                *state = SessionState::Running;
                drop(state);

                let response = ControlResponseOut::success(
                    request_id.clone(),
                    json!({
                        "decision": "approve",
                        "reason": "auto-approved after 30s timeout",
                    }),
                );
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = session.stdin_tx.send(json).await;
                }

                let timeout_msg = Arc::new(Message::Error {
                    message: format!(
                        "Hook timeout (request_id={}): auto-approved after 30s",
                        request_id
                    ),
                    code: "hook_timeout".to_string(),
                });
                let mut history = session.history.lock().await;
                history.push_back(timeout_msg.clone());
                while history.len() > MAX_HISTORY_SIZE {
                    history.pop_front();
                }
                drop(history);
                let _ = session.event_tx.send(timeout_msg);
            }
        }
    })
}
