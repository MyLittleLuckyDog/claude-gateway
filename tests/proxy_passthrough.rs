//! Request-passthrough measurements for the direct API proxy, run against a
//! local stub that stands in for api.anthropic.com. The stub records the body
//! of every request the proxy actually sends upstream, so these tests can say
//! exactly what the gateway adds, changes, or accumulates — without spending a
//! real API call.

mod common;

use claude_agent::proxy;
use claude_agent::proxy_session::{ProxySession, SessionOptions};
use common::{start, user_turns};
use serde_json::{json, Value};

// ── Measurements ───────────────────────────────────────────────────

/// What the caller sends in `messages` must be exactly what goes upstream, and
/// whatever the gateway adds on top must be a fixed overhead — not something
/// that grows with the number of requests.
#[tokio::test]
async fn messages_pass_through_unchanged_with_constant_overhead() {
    let (state, up) = start().await;
    let system = json!("You are a test agent.");

    let mut overheads = Vec::new();
    for round in 1..=6usize {
        let messages = user_turns(round * 2 - 1);
        let sent = json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 64,
            "system": system,
            "messages": messages,
        });
        let sent_len = serde_json::to_vec(&sent).expect("serialize").len();

        proxy::messages_sync(&state, sent, None, "sess-passthrough")
            .await
            .expect("messages_sync");

        let got = up
            .requests
            .lock()
            .expect("lock")
            .last()
            .cloned()
            .expect("request");
        assert_eq!(
            got["messages"],
            Value::Array(messages),
            "round {round}: messages were altered on the way upstream"
        );
        let got_len = serde_json::to_vec(&got).expect("serialize").len();
        overheads.push(got_len - sent_len);
    }

    let first = overheads[0];
    assert!(
        overheads.iter().all(|o| *o == first),
        "gateway overhead changed between requests: {overheads:?}"
    );
    eprintln!("[measure] constant gateway overhead per request: {first} bytes");
}

/// Same caller body, repeated: nothing the gateway injects may stack up
/// (system blocks, metadata), and the bytes upstream must be identical.
#[tokio::test]
async fn repeated_identical_requests_do_not_grow() {
    let (state, up) = start().await;
    for _ in 0..5 {
        let body = json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 64,
            "system": [{"type": "text", "text": "agent", "cache_control": {"type": "ephemeral"}}],
            "messages": user_turns(3),
        });
        proxy::messages_sync(&state, body, None, "sess-repeat")
            .await
            .expect("messages_sync");
    }
    let reqs = up.requests.lock().expect("lock");
    assert_eq!(reqs.len(), 5);
    for r in reqs.iter() {
        assert_eq!(r, &reqs[0], "upstream body drifted between identical calls");
        assert_eq!(r["system"].as_array().map(Vec::len), Some(2));
    }
}

/// Session mode re-sends the whole server-side history every turn, so its
/// request grows by exactly one user + one assistant message per turn.
/// This pins that growth so any extra accumulation shows up immediately.
#[tokio::test]
async fn session_mode_grows_by_exactly_one_exchange_per_turn() {
    let (state, up) = start().await;
    let mut session = ProxySession::new(
        "sess-mode".to_string(),
        SessionOptions {
            model: Some("claude-sonnet-5-5".to_string()),
            ..Default::default()
        },
        3600,
    );

    let mut counts = Vec::new();
    for turn in 0..5 {
        session.add_user_message(json!([{"type": "text", "text": format!("turn {turn}")}]));
        let body = session.build_request(None);
        let (resp, _) = proxy::messages_sync(&state, body, None, "sess-mode")
            .await
            .expect("messages_sync");
        session.record_assistant_response(&resp);
        counts.push(
            up.requests
                .lock()
                .expect("lock")
                .last()
                .and_then(|r| r["messages"].as_array().map(Vec::len))
                .unwrap_or(0),
        );
    }
    assert_eq!(counts, vec![1, 3, 5, 7, 9]);
}
