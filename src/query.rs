use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::error::GatewayError;
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
    pub cost_usd: Option<f64>,
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
    let mut transport = CliTransport::new(options, config.clone());
    transport.connect().await?;

    let mut event_rx = transport.event_receiver()
        .ok_or_else(|| GatewayError::Internal("No event receiver".to_string()))?;

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
) -> Result<(String, mpsc::Receiver<Message>), GatewayError> {
    let mut transport = CliTransport::new(options, config.clone());
    transport.connect().await?;

    let mut event_rx = transport.event_receiver()
        .ok_or_else(|| GatewayError::Internal("No event receiver".to_string()))?;

    // Send user message immediately
    let json = build_user_message(prompt)?;
    transport.write(&json).await?;

    let (msg_tx, msg_rx) = mpsc::channel::<Message>(256);
    let session_id = String::new(); // will be populated from init event

    // Spawn background task to relay events
    tokio::spawn(async move {
        let _sid = session_id;
        while let Some(event) = event_rx.recv().await {
            match event {
                Ok(CliOutputEvent::Unknown) => continue,
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

    // Return empty session_id initially — client gets it from system:init event in stream
    Ok((String::new(), msg_rx))
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
            subtype: format!("{:?}", r.subtype).to_lowercase(),
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
                    }
                }
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
