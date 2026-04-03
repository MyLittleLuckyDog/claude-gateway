pub mod server_rules;

use std::sync::Arc;

use crate::messages::Message;
use crate::messages::cli_input::{CliInputMessage, HookDecision};
use crate::messages::cli_output::CliHookRequestEvent;
use crate::options::ClaudeAgentOptions;
use crate::session::{Session, SessionState, MAX_HISTORY_SIZE};

use server_rules::evaluate_hook_rules;

/// Process a hook request: check server rules first, if none match, return None to defer to client.
/// Returns the hook_response JSON string to write to stdin, or None if deferred to client.
pub fn try_auto_resolve_hook(
    options: &ClaudeAgentOptions,
    hook: &CliHookRequestEvent,
) -> Option<String> {
    let decision = evaluate_hook_rules(options, hook)?;

    let resp = CliInputMessage::HookResponse {
        hook_id: hook.hook_id.clone(),
        decision,
        reason: None,
        updated_input: None,
        suppress_output: None,
    };

    serde_json::to_string(&resp).ok()
}

/// Start a hook timeout task that auto-approves after 30 seconds
pub fn spawn_hook_timeout(
    session: Arc<Session>,
    hook_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let mut state = session.state.lock().await;
        if let SessionState::WaitingForHook { hook_id: ref waiting_id, .. } = *state {
            if *waiting_id == hook_id {
                tracing::warn!("Hook {} timed out, auto-approving", hook_id);
                *state = SessionState::Running;
                drop(state);

                let resp = CliInputMessage::HookResponse {
                    hook_id: hook_id.clone(),
                    decision: HookDecision::Approve,
                    reason: Some("auto-approved after 30s timeout".to_string()),
                    updated_input: None,
                    suppress_output: None,
                };

                if let Ok(json) = serde_json::to_string(&resp) {
                    let _ = session.stdin_tx.send(json).await;
                }

                // Record timeout in history
                let timeout_msg = Arc::new(Message::Error {
                    message: format!("Hook timeout (hook_id={}): auto-approved after 30s", hook_id),
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
