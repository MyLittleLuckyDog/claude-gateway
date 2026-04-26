pub mod cli_control;
pub mod cli_input;
pub mod cli_output;
pub mod content;

// Re-export shared types from content
pub use content::{ContentBlock, SessionUsage, TokenUsage};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// SSE event and query() stream item
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    System {
        session_id: String,
        subtype: String,
        tools: Vec<Value>,
        model: Option<String>,
    },
    Assistant {
        session_id: String,
        parent_tool_use_id: Option<String>,
        message: AssistantMessage,
    },
    User {
        session_id: String,
        parent_tool_use_id: Option<String>,
        message: Value,
    },
    Result {
        session_id: String,
        subtype: String,
        result: Option<String>,
        error: Option<String>,
        cost_usd: Option<f64>,
        total_cost_usd: Option<f64>,
        usage: Option<SessionUsage>,
        num_turns: Option<u32>,
        duration_ms: Option<u64>,
    },
    StreamEvent {
        session_id: String,
        uuid: Option<String>,
        stream_event: Value,
    },
    /// A hook_callback control_request surfaced to streaming clients. The
    /// `request_id` lets the client respond via `/sessions/:id/hook_response`
    /// when the server has no matching rule (the defer path).
    HookRequest {
        request_id: String,
        callback_id: String,
        hook_event_name: String,
        tool_name: Option<String>,
        tool_input: Option<Value>,
        tool_use_id: Option<String>,
    },
    /// A can_use_tool control_request surfaced to streaming clients. Clients
    /// answer via `/sessions/:id/permission_response`.
    PermissionRequest {
        request_id: String,
        tool_name: String,
        input: Value,
        permission_suggestions: Option<Value>,
    },
    Error {
        message: String,
        code: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub id: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}
