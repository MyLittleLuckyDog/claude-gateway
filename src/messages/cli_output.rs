use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::cli_control::ControlRequest;
use super::content::{ContentBlock, SessionUsage, TokenUsage};

/// All event types read from CLI stdout
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliOutputEvent {
    System(CliSystemEvent),
    Assistant(CliAssistantEvent),
    User(CliUserEvent),
    Result(CliResultEvent),
    StreamEvent(CliStreamEventWrapper),
    /// Inbound control_request envelope (hook callbacks, tool permission asks, ...).
    /// Replaces the older top-level `hook_request` shape.
    ControlRequest(ControlRequest),
    /// Control response from CLI to a request we issued (e.g. initialize).
    /// Kept as raw value — the session loop matches request_id and moves on.
    ControlResponse(Value),
    /// Unknown event types (rate_limit_event, etc.) — ignored gracefully
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct CliSystemEvent {
    pub subtype: SystemSubtype,
    pub session_id: String,
    /// Tools can be Vec<String> (names) or Vec<ToolInfo> depending on CLI version
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SystemSubtype {
    Init,
    CompactBoundary,
    /// Unknown subtypes (hook_started, hook_response, etc.)
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct CliAssistantEvent {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub message: CliAssistantMessage,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CliAssistantMessage {
    pub id: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
pub struct CliUserEvent {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub message: Value,
}

#[derive(Debug, Deserialize)]
pub struct CliResultEvent {
    /// The CLI's own verdict on the turn: `success`, `error_max_turns`,
    /// `error_during_execution`, `error_max_budget_usd`,
    /// `error_max_structured_output_retries`, and whatever it adds next.
    ///
    /// Kept as the raw string on purpose. Nothing here branches on it — the
    /// gateway forwards the verdict rather than interpreting it — so a typed
    /// enum bought nothing and cost the whole line whenever the CLI used a name
    /// it didn't know. It did: three of the five names never matched, so every
    /// failed turn was dropped instead of reported.
    pub subtype: String,
    pub session_id: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub cost_usd: Option<f64>,
    pub total_cost_usd: Option<f64>,
    pub usage: Option<SessionUsage>,
    pub num_turns: Option<u32>,
    pub duration_ms: Option<u64>,
    pub duration_api_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CliStreamEventWrapper {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub uuid: Option<String>,
    /// The Messages API streaming event — `message_start`,
    /// `content_block_delta`, `message_stop`, and so on.
    ///
    /// The CLI names this field `event`; it is kept as `stream_event` here to
    /// match the name the gateway publishes to its own clients. `message_start`
    /// lines also carry `ttft_ms`, which is ignored.
    #[serde(rename = "event")]
    pub stream_event: Value,
}

#[derive(Debug, Deserialize, Clone, Serialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Parse a tools array that could be either Vec<String> or Vec<ToolInfo>
pub fn parse_tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| {
            t.as_str().map(|s| s.to_string()).or_else(|| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .map(|s| s.to_string())
            })
        })
        .collect()
}
