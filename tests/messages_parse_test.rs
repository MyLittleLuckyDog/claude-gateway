use claude_agent::messages::cli_output::*;

#[test]
fn test_parse_system_init() {
    let raw = include_str!("fixtures/system_init.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse system init");
    match event {
        CliOutputEvent::System(sys) => {
            assert_eq!(sys.subtype, SystemSubtype::Init);
            assert_eq!(sys.session_id, "550e8400-e29b-41d4-a716-446655440000");
            assert_eq!(sys.tools.len(), 2);
            assert_eq!(sys.tools[0]["name"].as_str(), Some("Read"));
            assert_eq!(sys.model.as_deref(), Some("claude-sonnet-4-6"));
        }
        _ => panic!("Expected System event"),
    }
}

#[test]
fn test_parse_assistant_text() {
    let raw = include_str!("fixtures/assistant_text.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse assistant text");
    match event {
        CliOutputEvent::Assistant(a) => {
            assert_eq!(a.message.id, "msg_01XFDUDYJgAACTU67reL2K");
            assert_eq!(a.message.content.len(), 1);
            assert_eq!(a.message.stop_reason.as_deref(), Some("end_turn"));
        }
        _ => panic!("Expected Assistant event"),
    }
}

#[test]
fn test_parse_assistant_tool_use() {
    let raw = include_str!("fixtures/assistant_tool_use.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse assistant tool_use");
    match event {
        CliOutputEvent::Assistant(a) => {
            assert_eq!(a.message.content.len(), 2);
            assert_eq!(a.message.stop_reason.as_deref(), Some("tool_use"));
        }
        _ => panic!("Expected Assistant event"),
    }
}

#[test]
fn test_parse_result_success() {
    let raw = include_str!("fixtures/result_success.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse result success");
    match event {
        CliOutputEvent::Result(r) => {
            assert_eq!(r.subtype, ResultSubtype::Success);
            assert_eq!(r.result.as_deref(), Some("The answer is 4."));
            assert_eq!(r.num_turns, Some(3));
        }
        _ => panic!("Expected Result event"),
    }
}

#[test]
fn test_parse_result_error() {
    let raw = include_str!("fixtures/result_error.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse result error");
    match event {
        CliOutputEvent::Result(r) => {
            assert_eq!(r.subtype, ResultSubtype::ErrorDuringGeneration);
            assert!(r.error.is_some());
        }
        _ => panic!("Expected Result event"),
    }
}

#[test]
fn test_parse_hook_request() {
    use claude_agent::messages::cli_control::{ControlRequestPayload, HookCallbackInput};
    let raw = include_str!("fixtures/hook_request.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse hook request");
    match event {
        CliOutputEvent::ControlRequest(ctl) => {
            assert_eq!(ctl.request_id, "req-uuid-001");
            match ctl.request {
                ControlRequestPayload::HookCallback(hc) => {
                    assert_eq!(hc.callback_id, "hook_0");
                    let input = HookCallbackInput::from_value(&hc.input);
                    assert_eq!(input.hook_event_name, "PreToolUse");
                    assert_eq!(input.tool_name.as_deref(), Some("Edit"));
                }
                _ => panic!("Expected hook_callback subtype"),
            }
        }
        _ => panic!("Expected ControlRequest event"),
    }
}

#[test]
fn test_parse_stream_event() {
    // Fixture is a real `--include-partial-messages` line, with only the ids
    // pinned. An earlier hand-written fixture used the wrong key for the
    // payload, so this test passed while every live partial frame failed to
    // parse — hence the assertions on the payload itself below.
    let raw = include_str!("fixtures/stream_event.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse stream event");
    match event {
        CliOutputEvent::StreamEvent(se) => {
            assert_eq!(se.uuid.as_deref(), Some("uuid-v4-123"));
            assert_eq!(se.session_id, "550e8400-e29b-41d4-a716-446655440000");
            assert_eq!(se.stream_event["type"], "content_block_delta");
            assert_eq!(se.stream_event["delta"]["text"], "1");
        }
        _ => panic!("Expected StreamEvent"),
    }
}

/// `message_start` carries a `ttft_ms` the other frames don't. An unknown
/// field must not fail the whole line.
#[test]
fn test_parse_stream_event_tolerates_extra_fields() {
    let raw = r#"{"type":"stream_event","event":{"type":"message_start"},
                  "session_id":"s","parent_tool_use_id":null,"uuid":"u","ttft_ms":812}"#;
    match serde_json::from_str::<CliOutputEvent>(raw).expect("parse message_start") {
        CliOutputEvent::StreamEvent(se) => {
            assert_eq!(se.stream_event["type"], "message_start");
        }
        _ => panic!("Expected StreamEvent"),
    }
}

// ── hook_request: does the client still own the decision? ──────────

use claude_agent::messages::Message;

fn hook_request(auto_resolved: bool) -> Message {
    Message::HookRequest {
        request_id: "req-1".to_string(),
        callback_id: "hook_0".to_string(),
        hook_event_name: "PreToolUse".to_string(),
        tool_name: Some("Bash".to_string()),
        tool_input: None,
        tool_use_id: None,
        auto_resolved,
    }
}

/// `auto_resolved: true` means a hook_rules entry already answered the CLI, so
/// the event is a record rather than a request — posting hook_response for it
/// would be rejected with 409.
#[test]
fn test_hook_request_reports_whether_it_was_auto_resolved() {
    let deferred = serde_json::to_value(hook_request(false)).unwrap();
    assert_eq!(deferred["type"], "hook_request");
    assert_eq!(deferred["auto_resolved"], false);

    let resolved = serde_json::to_value(hook_request(true)).unwrap();
    assert_eq!(resolved["auto_resolved"], true);
}

/// Payloads written before the field existed must still parse.
#[test]
fn test_hook_request_defaults_auto_resolved_when_absent() {
    let raw = r#"{"type":"hook_request","request_id":"r","callback_id":"hook_0",
                  "hook_event_name":"PreToolUse","tool_name":null,
                  "tool_input":null,"tool_use_id":null}"#;
    match serde_json::from_str::<Message>(raw).unwrap() {
        Message::HookRequest { auto_resolved, .. } => assert!(!auto_resolved),
        other => panic!("expected HookRequest, got {other:?}"),
    }
}
