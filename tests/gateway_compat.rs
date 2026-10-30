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

fn upstream_betas(up: &common::Upstream) -> Vec<String> {
    up.last_header("anthropic-beta")
        .unwrap_or_default()
        .split(',')
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect()
}

/// Real SDK clients ask for beta features with the `anthropic-beta` request
/// header (not a body field). The protocol document: pass `anthropic-beta`
/// values to the upstream unchanged.
#[tokio::test]
async fn anthropic_beta_request_header_reaches_upstream() {
    let (base, _proxy, up) = start_with_gateway().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        // One comma-separated line and one separate header line: both are legal.
        .header(
            "anthropic-beta",
            "structured-outputs-2025-12-15, fast-mode-2026-02-01",
        )
        .header("anthropic-beta", "task-budgets-2026-03-13")
        .json(&messages_body())
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);

    let betas = upstream_betas(&up);
    for wanted in [
        "structured-outputs-2025-12-15",
        "fast-mode-2026-02-01",
        "task-budgets-2026-03-13",
    ] {
        assert!(
            betas.contains(&wanted.to_string()),
            "{wanted} missing from {betas:?}"
        );
    }
    // The gateway's own required betas are still there.
    assert!(betas.contains(&"oauth-2025-04-20".to_string()), "{betas:?}");
}

/// Header betas and the gateway's older body `betas` field combine, without
/// repeating a value; the streaming path behaves the same.
#[tokio::test]
async fn header_and_body_betas_merge_without_duplicates_on_both_paths() {
    let (base, _proxy, up) = start_with_gateway().await;
    let client = reqwest::Client::new();

    for (path, stream) in [("/v1/messages", false), ("/v1/messages/stream", true)] {
        let mut body = messages_body();
        body["betas"] = json!(["fast-mode-2026-02-01", "from-body-2026-01-01"]);
        if stream {
            body["stream"] = json!(true);
        }
        let resp = client
            .post(format!("{base}{path}"))
            .header(
                "anthropic-beta",
                "fast-mode-2026-02-01,from-header-2026-01-01",
            )
            .json(&body)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), 200, "{path}");
        let _ = resp.bytes().await;

        let betas = upstream_betas(&up);
        for wanted in [
            "fast-mode-2026-02-01",
            "from-body-2026-01-01",
            "from-header-2026-01-01",
        ] {
            assert_eq!(
                betas.iter().filter(|b| b.as_str() == wanted).count(),
                1,
                "{path}: {wanted} should appear exactly once in {betas:?}"
            );
        }
    }
}
