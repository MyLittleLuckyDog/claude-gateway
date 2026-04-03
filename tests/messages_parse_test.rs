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
    let raw = include_str!("fixtures/hook_request.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse hook request");
    match event {
        CliOutputEvent::HookRequest(h) => {
            assert_eq!(h.hook_id, "hook-uuid-001");
            assert_eq!(h.hook_event_name, "PreToolUse");
            assert_eq!(h.tool_name.as_deref(), Some("Edit"));
        }
        _ => panic!("Expected HookRequest event"),
    }
}

#[test]
fn test_parse_stream_event() {
    let raw = include_str!("fixtures/stream_event.json");
    let event: CliOutputEvent = serde_json::from_str(raw).expect("parse stream event");
    match event {
        CliOutputEvent::StreamEvent(se) => {
            assert_eq!(se.uuid.as_deref(), Some("uuid-v4-123"));
        }
        _ => panic!("Expected StreamEvent"),
    }
}
