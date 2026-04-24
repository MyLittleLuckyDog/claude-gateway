//! CLI control protocol envelopes — mirrors claude-agent-sdk's _internal/query.py.
//!
//! The CLI expects hook, permission, and MCP messages to be wrapped in a
//! `control_request`/`control_response` envelope. The server must:
//!   1. Send a `control_request { subtype: "initialize", hooks: {...} }` right
//!      after spawning the CLI so the CLI knows to route PreToolUse events back.
//!   2. Handle incoming `control_request { subtype: "hook_callback", ... }`
//!      events by evaluating the associated rule and writing a matching
//!      `control_response { subtype: "success", request_id, response: {...} }`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Inbound (stdout) control_request from CLI.
///
/// Shape: `{"type":"control_request","request_id":"...","request":{...}}`.
#[derive(Debug, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    pub request: ControlRequestPayload,
}

/// Variants of the inner `request` object — discriminated by `subtype`.
#[derive(Debug, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequestPayload {
    HookCallback(HookCallbackRequest),
    CanUseTool(CanUseToolRequest),
    /// Any subtype we don't recognize is still surfaced so we can respond with
    /// an error control_response and keep the CLI unblocked.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct HookCallbackRequest {
    pub callback_id: String,
    /// Hook input payload (`hook_event_name`, `tool_name`, `tool_input`, ...).
    pub input: Value,
    /// Present for tool-scoped hooks (PreToolUse/PostToolUse).
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CanUseToolRequest {
    pub tool_name: String,
    pub input: Value,
    #[serde(default)]
    pub permission_suggestions: Option<Value>,
}

/// Outbound (stdin) control_response that we write back to the CLI.
///
/// Shape matches SDK exactly:
/// `{"type":"control_response","response":{"subtype":"success","request_id":"...","response":{...}}}`.
#[derive(Debug, Serialize)]
pub struct ControlResponseOut {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub response: ControlResponseInner,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ControlResponseInner {
    Success {
        subtype: &'static str, // "success"
        request_id: String,
        response: Value,
    },
    Error {
        subtype: &'static str, // "error"
        request_id: String,
        error: String,
    },
}

impl ControlResponseOut {
    pub fn success(request_id: String, payload: Value) -> Self {
        Self {
            msg_type: "control_response",
            response: ControlResponseInner::Success {
                subtype: "success",
                request_id,
                response: payload,
            },
        }
    }

    pub fn error(request_id: String, message: impl Into<String>) -> Self {
        Self {
            msg_type: "control_response",
            response: ControlResponseInner::Error {
                subtype: "error",
                request_id,
                error: message.into(),
            },
        }
    }
}

/// Outbound (stdin) control_request that the server initiates (e.g. initialize).
#[derive(Debug, Serialize)]
pub struct ControlRequestOut {
    #[serde(rename = "type")]
    pub msg_type: &'static str,
    pub request_id: String,
    pub request: Value,
}

impl ControlRequestOut {
    pub fn new(request_id: String, request: Value) -> Self {
        Self {
            msg_type: "control_request",
            request_id,
            request,
        }
    }
}

/// Hook-callback input extracted for evaluator convenience.
#[derive(Debug, Clone)]
pub struct HookCallbackInput {
    pub hook_event_name: String,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
}

impl HookCallbackInput {
    /// Extract from the raw `input` JSON sent by the CLI.
    pub fn from_value(input: &Value) -> Self {
        Self {
            hook_event_name: input
                .get("hook_event_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            tool_name: input
                .get("tool_name")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            tool_input: input.get("tool_input").cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_hook_callback_request() {
        let raw = json!({
            "request_id": "req_1",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "hook_1",
                "input": {
                    "hook_event_name": "PreToolUse",
                    "tool_name": "Bash",
                    "tool_input": {"command": "echo hi"}
                },
                "tool_use_id": "toolu_xyz"
            }
        });
        let ctl: ControlRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(ctl.request_id, "req_1");
        match ctl.request {
            ControlRequestPayload::HookCallback(h) => {
                assert_eq!(h.callback_id, "hook_1");
                let inp = HookCallbackInput::from_value(&h.input);
                assert_eq!(inp.hook_event_name, "PreToolUse");
                assert_eq!(inp.tool_name.as_deref(), Some("Bash"));
                assert_eq!(h.tool_use_id.as_deref(), Some("toolu_xyz"));
            }
            _ => panic!("expected hook_callback"),
        }
    }

    #[test]
    fn serializes_success_response_matching_sdk_shape() {
        let out = ControlResponseOut::success(
            "req_1".to_string(),
            json!({"decision": "block", "reason": "no bash"}),
        );
        let s = serde_json::to_string(&out).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "req_1");
        assert_eq!(v["response"]["response"]["decision"], "block");
    }

    #[test]
    fn unknown_subtype_is_captured_gracefully() {
        let raw = json!({
            "request_id": "req_x",
            "request": {"subtype": "future_feature_we_dont_know"}
        });
        let ctl: ControlRequest = serde_json::from_value(raw).unwrap();
        assert!(matches!(ctl.request, ControlRequestPayload::Unknown));
    }
}
