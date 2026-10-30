//! HTTP-level checks of the gateway as an Anthropic-compatible endpoint: a real
//! client talks to the real router, which talks to a local upstream stub.
//! The expectations follow the "Claude Code gateway protocol" document that
//! ships inside the Claude Code CLI bundle.

mod common;

use common::start_with_gateway;
use serde_json::{json, Value};

fn messages_body() -> Value {
    json!({
        "model": "claude-sonnet-5-5",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

/// Baseline: a plain request goes through the router to upstream and back.
#[tokio::test]
async fn plain_messages_request_round_trips() {
    let (base, _proxy, up) = start_with_gateway().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_body())
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(up.request_count(), 1);
}
