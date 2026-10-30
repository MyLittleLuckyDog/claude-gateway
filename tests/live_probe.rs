//! One-off acceptance probe against the REAL Anthropic API, using the
//! developer's own OAuth credentials. Answers a single question the stub
//! cannot: does subscription (OAuth) traffic accept `cache_control` markers
//! and the `context_management` field + beta header, and does the cache
//! marker actually produce a write then a read?
//!
//! Never runs by default. Spends a handful of tiny requests:
//!
//!     cargo test --test live_probe -- --ignored --nocapture
//!
//! Prints status and usage only — never credentials or headers.

use std::sync::Arc;

use claude_agent::proxy::{self, ProxyState};
use claude_agent::proxy_session::{ProxySession, SessionOptions};
use serde_json::{json, Value};

/// ~2k tokens of text that is stable across calls, so the second call can read
/// what the first one wrote. Above every model's minimum cacheable prefix.
fn filler() -> String {
    (0..220)
        .map(|i| {
            format!("Reference item {i}: the quick brown fox number {i} jumps over lazy dog {i}.")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn summarize(label: &str, result: Result<(Value, u16), proxy::ProxyError>) -> Option<Value> {
    match result {
        Ok((body, 200)) => {
            eprintln!(
                "[probe] {label}: 200 stop_reason={} usage={} applied_edits={}",
                body["stop_reason"], body["usage"], body["context_management"]["applied_edits"],
            );
            Some(body)
        }
        Ok((body, status)) => {
            eprintln!(
                "[probe] {label}: HTTP {status} error={}",
                body["error"]["message"]
            );
            None
        }
        Err(e) => {
            eprintln!("[probe] {label}: gateway error: {e}");
            None
        }
    }
}

async fn session_call(
    state: &Arc<ProxyState>,
    model: &str,
    cache_breakpoint: bool,
    text: &str,
    betas: Option<&[String]>,
    extra: Option<(&str, Value)>,
) -> Result<(Value, u16), proxy::ProxyError> {
    let mut session = ProxySession::new(
        "live-probe".to_string(),
        SessionOptions {
            model: Some(model.to_string()),
            max_tokens: Some(64),
            cache_breakpoint,
            ..Default::default()
        },
        3600,
    );
    session.add_user_message(json!([{"type": "text", "text": text}]));
    let mut body = session.build_request(None);
    if let Some((key, value)) = extra {
        body[key] = value;
    }
    proxy::messages_sync(state, body, betas, "live-probe").await
}

#[tokio::test]
#[ignore = "calls the real Anthropic API with the developer's credentials"]
async fn oauth_traffic_accepts_cache_markers_and_context_management() {
    let state = Arc::new(ProxyState::new(1));
    let prompt = format!("{}\n\nReply with the single word: ok", filler());

    // 1. Cheap family: marker accepted at all?
    summarize(
        "haiku-5-5 + cache marker",
        session_call(&state, "claude-haiku-5-5", true, &prompt, None, None).await,
    );

    // 2. The family the gateway is actually used with: first call writes…
    let first = summarize(
        "sonnet-5-5 + cache marker (expect cache write)",
        session_call(&state, "claude-sonnet-5-5", true, &prompt, None, None).await,
    );
    // …and an identical second call reads.
    let second = summarize(
        "sonnet-5-5 + cache marker again (expect cache read)",
        session_call(&state, "claude-sonnet-5-5", true, &prompt, None, None).await,
    );
    if let (Some(a), Some(b)) = (first, second) {
        let wrote = a["usage"]["cache_creation_input_tokens"]
            .as_u64()
            .unwrap_or(0);
        let read = b["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0);
        eprintln!("[probe] cache write on 1st = {wrote}, cache read on 2nd = {read}");
    }

    // 3. Control: same request without the marker should not touch the cache.
    summarize(
        "sonnet-5-5 without marker (control)",
        session_call(
            &state,
            "claude-sonnet-5-5",
            false,
            "Reply with the single word: ok",
            None,
            None,
        )
        .await,
    );

    // 4. context_management + its beta header on subscription traffic.
    let betas = vec!["context-management-2025-06-27".to_string()];
    summarize(
        "sonnet-5-5 + context_management clear_tool_uses (beta header)",
        session_call(
            &state,
            "claude-sonnet-5-5",
            false,
            "Reply with the single word: ok",
            Some(&betas),
            Some((
                "context_management",
                json!({"edits": [{
                    "type": "clear_tool_uses_20250919",
                    "trigger": {"type": "input_tokens", "value": 100000},
                    "keep": {"type": "tool_uses", "value": 5},
                    "clear_at_least": {"type": "input_tokens", "value": 20000},
                }]}),
            )),
        )
        .await,
    );
}

/// Does the server take part in Claude Code's `context_hint` protocol for this
/// account? The CLI sends beta `context-hint-2026-04-09` plus
/// `context_hint: {enabled: true, target_tokens_saved: N}` and reacts to a
/// 422/424 rejection by clearing old tool results and retrying; a 400 means
/// "not supported, carry on without it". Two tiny requests tell us which.
#[tokio::test]
#[ignore = "calls the real Anthropic API with the developer's credentials"]
async fn server_participation_in_context_hint_protocol() {
    let state = Arc::new(ProxyState::new(1));
    let betas = vec!["context-hint-2026-04-09".to_string()];

    for (label, hint) in [
        (
            "sonnet-5-5 + context_hint {enabled, target_tokens_saved:75000}",
            json!({"enabled": true, "target_tokens_saved": 75000}),
        ),
        (
            "sonnet-5-5 + context_hint {enabled} (no target)",
            json!({"enabled": true}),
        ),
    ] {
        summarize(
            label,
            session_call(
                &state,
                "claude-sonnet-5-5",
                false,
                "Reply with the single word: ok",
                Some(&betas),
                Some(("context_hint", hint)),
            )
            .await,
        );
    }
}
