use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::error::GatewayError;
use crate::hooks::{self, AutoResolveOutcome};
use crate::messages::cli_control::{
    ControlRequest, ControlRequestOut, ControlRequestPayload, ControlResponseOut,
};
use crate::messages::cli_input::{CliInputMessage, CliUserInput, InputContent};
use crate::messages::cli_output::{CliOutputEvent, CliResultEvent, SystemSubtype};
use crate::messages::{AssistantMessage, Message, SessionUsage};
use crate::options::ClaudeAgentOptions;
use crate::transport::cli::CliTransport;
use crate::transport::Transport;

/// Result of a single query() call
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    pub session_id: String,
    pub result: Option<String>,
    pub subtype: String,
    /// Per-turn cost. The CLI stopped populating this — prefer
    /// `total_cost_usd`.
    pub cost_usd: Option<f64>,
    /// Running cost for the CLI session behind this call. A stateless query
    /// gets its own CLI session, so this is the cost of this call.
    pub total_cost_usd: Option<f64>,
    pub usage: Option<SessionUsage>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
}

/// Build the user message JSON string
fn build_user_message(prompt: &str) -> Result<String, GatewayError> {
    let msg = CliInputMessage::User {
        message: CliUserInput {
            role: "user".to_string(),
            content: vec![InputContent::Text {
                text: prompt.to_string(),
            }],
        },
    };
    serde_json::to_string(&msg)
        .map_err(|e| GatewayError::Internal(format!("JSON serialize error: {}", e)))
}

/// Execute a single-turn query: spawn CLI, send prompt immediately, collect result, close.
///
/// Key insight: the CLI needs the user message on stdin BEFORE it produces system:init.
/// Flow: connect -> write user msg -> read events (init, assistant, result) -> close
pub async fn query(
    prompt: &str,
    options: ClaudeAgentOptions,
    config: &AppConfig,
) -> Result<QueryResult, GatewayError> {
    tracing::debug!("query: spawning CLI transport");
    let mut transport = CliTransport::new(options.clone(), config.clone());
    transport.connect().await?;

    let mut event_rx = transport
        .event_receiver()
        .ok_or_else(|| GatewayError::Internal("No event receiver".to_string()))?;

    send_initialize_if_needed(&transport, &options).await?;

    // Send user message IMMEDIATELY (CLI reads stdin before producing output)
    let json = build_user_message(prompt)?;
    transport.write(&json).await?;
    tracing::debug!("query: user message sent, waiting for events");

    // Collect events until result
    let mut session_id = String::new();
    let mut result_event: Option<CliResultEvent> = None;

    while let Some(event) = event_rx.recv().await {
        match event {
            Ok(CliOutputEvent::System(sys)) if sys.subtype == SystemSubtype::Init => {
                tracing::debug!("query: received system:init, session_id={}", sys.session_id);
                session_id = sys.session_id;
            }
            Ok(CliOutputEvent::Result(r)) => {
                if session_id.is_empty() {
                    session_id = r.session_id.clone();
                }
                result_event = Some(r);
                break;
            }
            Ok(CliOutputEvent::ControlRequest(ctl)) => {
                let _ = handle_stateless_control_request(&transport, &options, ctl).await?;
                continue;
            }
            Ok(CliOutputEvent::ControlResponse(_)) => continue,
            Ok(CliOutputEvent::Unknown) => continue,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!("query event error: {}", e);
                continue;
            }
        }
    }

    transport.close().await?;

    match result_event {
        Some(r) => Ok(QueryResult {
            session_id,
            result: r.result,
            subtype: format!("{:?}", r.subtype).to_lowercase(),
            cost_usd: r.cost_usd,
            total_cost_usd: r.total_cost_usd,
            usage: r.usage,
            num_turns: r.num_turns,
            duration_ms: r.duration_ms,
        }),
        None => Err(GatewayError::CliConnection(
            "CLI process exited without result".to_string(),
        )),
    }
}

/// Stream version: returns a channel of Messages for SSE streaming.
/// Sends user message immediately, then relays all events through channel.
pub async fn query_stream(
    prompt: &str,
    options: ClaudeAgentOptions,
    config: &AppConfig,
) -> Result<mpsc::Receiver<Message>, GatewayError> {
    let mut transport = CliTransport::new(options.clone(), config.clone());
    transport.connect().await?;

    let mut event_rx = transport
        .event_receiver()
        .ok_or_else(|| GatewayError::Internal("No event receiver".to_string()))?;

    send_initialize_if_needed(&transport, &options).await?;

    // Send user message immediately
    let json = build_user_message(prompt)?;
    transport.write(&json).await?;

    let (msg_tx, msg_rx) = mpsc::channel::<Message>(256);

    // Spawn background task to relay events
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                Ok(CliOutputEvent::Unknown) => continue,
                Ok(CliOutputEvent::ControlRequest(ctl)) => {
                    match handle_stateless_control_request(&transport, &options, ctl).await {
                        Ok(Some(msg)) => {
                            if msg_tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let err_msg = Message::Error {
                                message: e.to_string(),
                                code: e.error_code().to_string(),
                            };
                            let _ = msg_tx.send(err_msg).await;
                            break;
                        }
                    }
                    continue;
                }
                Ok(CliOutputEvent::ControlResponse(_)) => continue,
                Ok(cli_event) => {
                    let is_result = matches!(cli_event, CliOutputEvent::Result(_));
                    let message = cli_output_to_message(cli_event);
                    if msg_tx.send(message).await.is_err() {
                        break;
                    }
                    if is_result {
                        break;
                    }
                }
                Err(e) => {
                    let err_msg = Message::Error {
                        message: e.to_string(),
                        code: e.error_code().to_string(),
                    };
                    let _ = msg_tx.send(err_msg).await;
                    break;
                }
            }
        }
        let _ = transport.close().await;
    });

    Ok(msg_rx)
}

/// Convert CliOutputEvent to public Message
pub fn cli_output_to_message(event: CliOutputEvent) -> Message {
    match event {
        CliOutputEvent::System(sys) => Message::System {
            session_id: sys.session_id,
            subtype: match sys.subtype {
                SystemSubtype::Init => "init".to_string(),
                SystemSubtype::CompactBoundary => "compact_boundary".to_string(),
                SystemSubtype::Other => "other".to_string(),
            },
            tools: sys.tools,
            model: sys.model,
        },
        CliOutputEvent::Assistant(a) => Message::Assistant {
            session_id: a.session_id,
            parent_tool_use_id: a.parent_tool_use_id,
            message: AssistantMessage {
                id: a.message.id,
                role: a.message.role,
                content: a.message.content,
                model: a.message.model,
                stop_reason: a.message.stop_reason,
                usage: a.message.usage,
            },
        },
        CliOutputEvent::User(u) => Message::User {
            session_id: u.session_id,
            parent_tool_use_id: u.parent_tool_use_id,
            message: u.message,
        },
        CliOutputEvent::Result(r) => Message::Result {
            session_id: r.session_id,
            subtype: r.subtype,
            result: r.result,
            error: r.error,
            cost_usd: r.cost_usd,
            total_cost_usd: r.total_cost_usd,
            usage: r.usage,
            num_turns: r.num_turns,
            duration_ms: r.duration_ms,
        },
        CliOutputEvent::StreamEvent(se) => Message::StreamEvent {
            session_id: se.session_id,
            uuid: se.uuid,
            stream_event: se.stream_event,
        },
        CliOutputEvent::ControlRequest(ctl) => {
            use crate::messages::cli_control::{ControlRequestPayload, HookCallbackInput};
            match ctl.request {
                ControlRequestPayload::HookCallback(hc) => {
                    let input = HookCallbackInput::from_value(&hc.input);
                    Message::HookRequest {
                        request_id: ctl.request_id,
                        callback_id: hc.callback_id,
                        hook_event_name: input.hook_event_name,
                        tool_name: input.tool_name,
                        tool_input: input.tool_input,
                        tool_use_id: hc.tool_use_id,
                        // The converter cannot know; the session loop, which
                        // owns the rule evaluation, sets this.
                        auto_resolved: false,
                    }
                }
                ControlRequestPayload::CanUseTool(req) => Message::PermissionRequest {
                    request_id: ctl.request_id,
                    tool_name: req.tool_name,
                    input: req.input,
                    permission_suggestions: req.permission_suggestions,
                },
                _ => Message::Error {
                    message: "Unsupported control_request subtype".to_string(),
                    code: "unsupported_control".to_string(),
                },
            }
        }
        CliOutputEvent::ControlResponse(_) => Message::Error {
            message: "Unexpected control_response in stream".to_string(),
            code: "unexpected_control_response".to_string(),
        },
        CliOutputEvent::Unknown => Message::Error {
            message: "Unknown CLI event type".to_string(),
            code: "unknown_event".to_string(),
        },
    }
}

async fn send_initialize_if_needed<T: Transport>(
    transport: &T,
    options: &ClaudeAgentOptions,
) -> Result<(), GatewayError> {
    if let Some((payload, _)) = hooks::build_initialize_request(options) {
        let request = ControlRequestOut::new(format!("init-{}", uuid::Uuid::new_v4()), payload);
        let json = serde_json::to_string(&request)
            .map_err(|e| GatewayError::Internal(format!("JSON serialize error: {}", e)))?;
        transport.write(&json).await?;
    }
    Ok(())
}

async fn handle_stateless_control_request<T: Transport>(
    transport: &T,
    options: &ClaudeAgentOptions,
    ctl: ControlRequest,
) -> Result<Option<Message>, GatewayError> {
    match ctl.request {
        ControlRequestPayload::HookCallback(callback) => {
            match hooks::try_auto_resolve_hook(options, &ctl.request_id, &callback) {
                AutoResolveOutcome::Respond(json) => {
                    transport.write(&json).await?;
                    Ok(None)
                }
                AutoResolveOutcome::DeferToClient => {
                    let response = ControlResponseOut::success(
                        ctl.request_id,
                        serde_json::json!({
                            "decision": "block",
                            "reason": "stateless /query cannot service deferred hook callbacks; use /sessions instead",
                        }),
                    );
                    let json = serde_json::to_string(&response).map_err(|e| {
                        GatewayError::Internal(format!("JSON serialize error: {}", e))
                    })?;
                    transport.write(&json).await?;
                    Ok(Some(Message::Error {
                        message: "Deferred hook callback auto-blocked in stateless /query"
                            .to_string(),
                        code: "stateless_hook_callback".to_string(),
                    }))
                }
            }
        }
        ControlRequestPayload::CanUseTool(req) => {
            let response = ControlResponseOut::success(
                ctl.request_id,
                serde_json::json!({
                    "behavior": "deny",
                    "message": "stateless /query cannot answer tool permission prompts; use /sessions instead",
                    "tool_name": req.tool_name,
                }),
            );
            let json = serde_json::to_string(&response)
                .map_err(|e| GatewayError::Internal(format!("JSON serialize error: {}", e)))?;
            transport.write(&json).await?;
            Ok(Some(Message::Error {
                message: "Tool permission prompt auto-denied in stateless /query".to_string(),
                code: "stateless_permission_prompt".to_string(),
            }))
        }
        ControlRequestPayload::Unknown => {
            let response = ControlResponseOut::error(
                ctl.request_id,
                "subtype not supported by stateless /query",
            );
            let json = serde_json::to_string(&response)
                .map_err(|e| GatewayError::Internal(format!("JSON serialize error: {}", e)))?;
            transport.write(&json).await?;
            Ok(Some(Message::Error {
                message: "Unsupported control_request subtype in stateless /query".to_string(),
                code: "unsupported_control".to_string(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{cli_output_to_message, handle_stateless_control_request};
    use crate::error::GatewayError;
    use crate::messages::cli_control::ControlRequest;
    use crate::messages::cli_output::CliOutputEvent;
    use crate::messages::Message;
    use crate::options::{ClaudeAgentOptions, HookAction, HookRule};
    use crate::transport::Transport;
    use tokio::sync::mpsc;

    struct RecordingTransport {
        writes: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Transport for RecordingTransport {
        async fn connect(&mut self) -> Result<(), GatewayError> {
            Ok(())
        }
        async fn write(&self, data: &str) -> Result<(), GatewayError> {
            self.writes.lock().unwrap().push(data.to_string());
            Ok(())
        }
        async fn close(&mut self) -> Result<(), GatewayError> {
            Ok(())
        }
        fn is_ready(&self) -> bool {
            true
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
        fn event_receiver(
            &mut self,
        ) -> Option<mpsc::Receiver<Result<CliOutputEvent, GatewayError>>> {
            None
        }
    }

    #[test]
    fn converts_can_use_tool_to_permission_request_message() {
        let raw = json!({
            "request_id": "req_perm_1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "git status"},
                "permission_suggestions": {"allow": false}
            }
        });
        let ctl: ControlRequest = serde_json::from_value(raw).unwrap();

        let message = cli_output_to_message(CliOutputEvent::ControlRequest(ctl));
        match message {
            Message::PermissionRequest {
                request_id,
                tool_name,
                input,
                permission_suggestions,
            } => {
                assert_eq!(request_id, "req_perm_1");
                assert_eq!(tool_name, "Bash");
                assert_eq!(input["command"], "git status");
                assert_eq!(permission_suggestions.unwrap()["allow"], false);
            }
            other => panic!("expected permission request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stateless_query_blocks_deferred_hook_callbacks() {
        let transport = RecordingTransport {
            writes: std::sync::Mutex::new(Vec::new()),
        };
        let raw = json!({
            "request_id": "req_hook_1",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "hook_1",
                "input": {
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "rm -rf /"}
                }
            }
        });
        let ctl: ControlRequest = serde_json::from_value(raw).unwrap();
        let options = ClaudeAgentOptions {
            hook_rules: Some(vec![HookRule {
                event: "PreToolUse".to_string(),
                tool_pattern: Some("Bash".to_string()),
                action: HookAction::Defer,
            }]),
            ..Default::default()
        };

        let msg = handle_stateless_control_request(&transport, &options, ctl)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg, Message::Error { .. }));

        let writes = transport.writes.lock().unwrap();
        let response: serde_json::Value = serde_json::from_str(&writes[0]).unwrap();
        assert_eq!(response["response"]["response"]["decision"], "block");
    }
}
