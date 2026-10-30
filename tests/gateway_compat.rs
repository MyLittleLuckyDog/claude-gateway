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

/// The official SDKs and the Claude Code CLI always POST `/v1/messages` with
/// `stream: true` and expect an SSE feed back. (The protocol document: "Don't
/// buffer SSE on the `stream: true` path.")
#[tokio::test]
async fn stream_true_on_v1_messages_returns_an_sse_feed() {
    let (base, _proxy, up) = start_with_gateway().await;
    let mut body = messages_body();
    body["stream"] = json!(true);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected an SSE response, got content-type {content_type:?}"
    );
    let text = resp.text().await.expect("body");
    assert!(
        text.contains("event: message_start"),
        "no message_start in {text:?}"
    );
    assert!(
        text.contains("event: message_stop"),
        "no message_stop in {text:?}"
    );
    assert_eq!(up.requests.lock().expect("lock")[0]["stream"], true);
}
