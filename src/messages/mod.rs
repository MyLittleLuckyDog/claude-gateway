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
        /// Wall time for the turn, as the CLI measures it. Excludes the CLI's
        /// own startup, so on a session's first turn it runs ~400ms short of
        /// what the caller sees.
        duration_ms: Option<u64>,
        /// Time the CLI spent waiting on the API. Subtracting it from
        /// `duration_ms` is the only way a client can tell model latency from
        /// gateway latency.
        duration_api_ms: Option<u64>,
    },
    StreamEvent {
        session_id: String,
        uuid: Option<String>,
        stream_event: Value,
    },
    /// A hook_callback control_request surfaced to streaming clients.
    ///
    /// Check `auto_resolved` before acting. When it is `false` the session is
    /// parked in `waiting_for_hook` and the client owns the decision — answer
    /// via `/sessions/:id/hook_response` with `request_id` before
    /// `hook_timeout_secs` elapses. When it is `true` a server-side
    /// `hook_rules` entry already answered the CLI and the event is only a
    /// record of that; responding to it returns `409 invalid_state`.
    HookRequest {
        request_id: String,
        callback_id: String,
        hook_event_name: String,
        tool_name: Option<String>,
        tool_input: Option<Value>,
        tool_use_id: Option<String>,
        /// `true` when a `hook_rules` entry already answered this callback.
        #[serde(default)]
        auto_resolved: bool,
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
