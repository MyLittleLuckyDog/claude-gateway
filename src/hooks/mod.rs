pub mod server_rules;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::core::events::record_and_broadcast;
use crate::messages::cli_control::{ControlResponseOut, HookCallbackInput, HookCallbackRequest};
use crate::messages::Message;
use crate::options::{ClaudeAgentOptions, HookTimeoutAction};
use crate::session::{Session, SessionState};

use server_rules::{evaluate_hook_rules, ResolvedDecision};

/// Build the `initialize` control_request payload and the per-session
/// callback_id → rule map. Returns `None` when no initialize-time data exists.
///
/// Hooks shape produced (matches SDK):
/// ```text
/// {
///   "PreToolUse": [
///     {"matcher": "Bash", "hookCallbackIds": ["hook_0"]},
///     {"matcher": "Read", "hookCallbackIds": ["hook_1"]}
///   ]
/// }
/// ```
pub fn build_initialize_request(
    options: &ClaudeAgentOptions,
) -> Option<(Value, HashMap<String, usize>)> {
    let mut request = serde_json::Map::new();
    request.insert(
        "subtype".to_string(),
        Value::String("initialize".to_string()),
    );

    let mut callback_map: HashMap<String, usize> = HashMap::new();
    if let Some(rules) = options.hook_rules.as_ref() {
        if !rules.is_empty() {
            let mut by_event: HashMap<String, Vec<Value>> = HashMap::new();
            for (idx, rule) in rules.iter().enumerate() {
                let callback_id = format!("hook_{}", idx);
                callback_map.insert(callback_id.clone(), idx);
                let matcher = rule.tool_pattern.clone().unwrap_or_else(|| "*".to_string());
                by_event.entry(rule.event.clone()).or_default().push(json!({
                    "matcher": matcher,
                    "hookCallbackIds": [callback_id],
                }));
            }
            request.insert("hooks".to_string(), json!(by_event));
        }
    }

    if let Some(agents) = options.agents.as_ref() {
        if !agents.is_empty() {
            request.insert("agents".to_string(), json!(agents));
        }
    }

    if request.len() == 1 {
        return None;
    }

    Some((Value::Object(request), callback_map))
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

/// Start a watchdog for a deferred hook callback. The timeout behavior is
/// request-scoped via session options so callers can choose block vs approve
/// per task rather than relying on a fixed global policy.
pub fn spawn_hook_timeout(
    session: Arc<Session>,
    request_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let timeout_secs = session.options.hook_timeout_secs.unwrap_or(30);
        let timeout_action = session
            .options
            .hook_timeout_action
            .clone()
            .unwrap_or(HookTimeoutAction::Block);

        tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;

        let mut state = session.state.lock().await;
        if let SessionState::WaitingForHook {
            request_id: ref waiting_id,
            ..
        } = *state
        {
            if *waiting_id == request_id {
                tracing::warn!(
                    "Hook request {} timed out after {}s, auto-{}",
                    request_id,
                    timeout_secs,
                    timeout_action.as_decision()
                );
                *state = SessionState::Running;
                drop(state);

                let (decision, reason) = match timeout_action {
                    HookTimeoutAction::Approve => (
                        "approve",
                        format!("auto-approved after {}s timeout", timeout_secs),
                    ),
                    HookTimeoutAction::Block => (
                        "block",
                        format!("auto-blocked after {}s timeout", timeout_secs),
                    ),
                };

                let response = ControlResponseOut::success(
                    request_id.clone(),
                    json!({
                        "decision": decision,
                        "reason": reason,
                    }),
                );
                if let Ok(json) = serde_json::to_string(&response) {
                    let _ = session.stdin_tx.send(json).await;
                }

                let timeout_msg = Arc::new(Message::Error {
                    message: format!(
                        "Hook timeout (request_id={}): auto-{} after {}s",
                        request_id,
                        timeout_action.as_decision(),
                        timeout_secs
                    ),
                    code: "hook_timeout".to_string(),
                });
                record_and_broadcast(&session.history, &session.event_tx, timeout_msg).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{build_initialize_request, HookTimeoutAction};
    use crate::options::{AgentDefinition, ClaudeAgentOptions, HookAction, HookRule};

    #[test]
    fn initialize_request_includes_hooks_and_agents() {
        let mut agents = HashMap::new();
        agents.insert(
            "reviewer".to_string(),
            AgentDefinition {
                description: Some("Reviews code".to_string()),
                prompt: Some("Review changes".to_string()),
                tools: Some(vec!["Read".to_string()]),
                model: Some("sonnet".to_string()),
            },
        );

        let opts = ClaudeAgentOptions {
            hook_rules: Some(vec![HookRule {
                event: "PreToolUse".to_string(),
                tool_pattern: Some("Bash".to_string()),
                action: HookAction::Defer,
            }]),
            agents: Some(agents),
            ..Default::default()
        };

        let (request, callback_map) = build_initialize_request(&opts).unwrap();
        assert_eq!(request["subtype"], "initialize");
        assert_eq!(request["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(request["agents"]["reviewer"]["prompt"], "Review changes");
        assert_eq!(callback_map.get("hook_0"), Some(&0));
    }

    #[test]
    fn hook_timeout_action_defaults_to_block() {
        let opts = ClaudeAgentOptions::default();
        assert!(matches!(
            opts.hook_timeout_action.unwrap_or(HookTimeoutAction::Block),
            HookTimeoutAction::Block
        ));
    }
}
