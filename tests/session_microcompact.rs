//! Microcompact of proxy sessions, end to end: real router, real session store,
//! upstream stub. Token figures come from the stub's `count_tokens`, which here
//! counts the way a server does (see `Upstream::count_by_content`); the gateway
//! itself never estimates a size.

mod common;

use std::sync::atomic::Ordering::Relaxed;

use common::{start_with_gateway, Scripted, Upstream};
use serde_json::{json, Value};

const MODEL: &str = "claude-sonnet-5-5";

/// A reply that ends in a `Read` tool call, with a context big enough (150,000
/// tokens, as the server reports it) to reach the session's trigger.
fn tool_use_reply(id: &str) -> Scripted {
    json!({
        "id": "msg_tool", "type": "message", "role": "assistant", "model": MODEL,
        "content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"path": "/x"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 150_000, "output_tokens": 10},
    })
    .into()
}

fn final_reply() -> Scripted {
    json!({
        "id": "msg_done", "type": "message", "role": "assistant", "model": MODEL,
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 150_000, "output_tokens": 10},
    })
    .into()
}

async fn post(client: &reqwest::Client, url: String, body: Value) -> Value {
    let resp = client.post(url).json(&body).send().await.expect("request");
    // 200 for a message, 201 for a created session.
    assert!(resp.status().is_success(), "{}", resp.status());
    resp.json().await.expect("json body")
}

/// Run a session through `calls` tool calls, each result `result_chars` long,
/// and return the response to the last message plus the messages request that
/// produced it.
async fn run(
    up: &Upstream,
    base: &str,
    calls: usize,
    result: impl Fn(usize) -> Value,
) -> (Value, Value) {
    let client = reqwest::Client::new();
    let created: Value = post(
        &client,
        format!("{base}/v1/sessions"),
        json!({"model": MODEL, "microcompact": {"trigger_tokens": 100_000}}),
    )
    .await;
    let id = created["id"].as_str().expect("session id").to_string();
    let url = format!("{base}/v1/sessions/{id}/msg");

    up.push(tool_use_reply("toolu_0"));
    post(&client, url.clone(), json!({"content": "start"})).await;
    let mut last = Value::Null;
    for i in 0..calls {
        up.push(if i + 1 < calls {
            tool_use_reply(&format!("toolu_{}", i + 1))
        } else {
            final_reply()
        });
        last = post(
            &client,
            url.clone(),
            json!({"is_tool_result": true, "content": [
                {"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": result(i)}
            ]}),
        )
        .await;
    }
    let request = up
        .requests
        .lock()
        .expect("lock")
        .iter()
        .rev()
        .find(|r| r.get("max_tokens").is_some())
        .cloned()
        .expect("a messages request");
    (last, request)
}

/// The content the last messages request carried for each tool result.
fn tool_results(request: &Value) -> Vec<(String, Value)> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            (
                b["tool_use_id"].as_str().unwrap_or("").to_string(),
                b["content"].clone(),
            )
        })
        .collect()
}

const PLACEHOLDER: &str = "[Old tool result content cleared]";

/// Three stale results of 10,000 tokens each: upstream counts 30,007 for their
/// content and 106 for the three placeholders, so clearing frees exactly
/// 29,901 -- the per-message overhead cancels -- which clears the 20,000 floor.
#[tokio::test]
async fn old_results_are_cleared_when_upstream_counts_enough_freed() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.count_by_content.store(true, Relaxed);

    let (last, request) = run(&up, &base, 8, |_| json!("x".repeat(10_000))).await;

    let results = tool_results(&request);
    assert_eq!(results.len(), 8);
    for (id, content) in &results[..3] {
        assert_eq!(content, PLACEHOLDER, "{id} should be cleared");
    }
    for (id, content) in &results[3..] {
        assert_eq!(
            content.as_str().map(str::len),
            Some(10_000),
            "{id} should be kept"
        );
    }
    assert_eq!(
        last["microcompact"],
        json!({"cleared_tool_results": 3, "tokens_freed": 29_901})
    );
}

/// 18,000 tokens of stale content frees 17,901: under the floor, so nothing is
/// cleared -- and an earlier turn with fewer stale results did not clear its
/// share either, which would have broken the cache for nothing.
#[tokio::test]
async fn nothing_is_cleared_below_the_floor() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.count_by_content.store(true, Relaxed);

    let (last, request) = run(&up, &base, 8, |_| json!("x".repeat(6_000))).await;

    for (id, content) in tool_results(&request) {
        assert_eq!(
            content.as_str().map(str::len),
            Some(6_000),
            "{id} must be intact"
        );
    }
    assert!(last.get("microcompact").is_none(), "{last}");
}

/// Images are counted by upstream (1,000 each here), not by a flat guess. One
/// stale result of 30 images is 30,000 tokens -- already over the floor -- so
/// each result is cleared on the turn it turns stale: 30,007 counted for the
/// content, 40 for its placeholder, 29,967 freed. By the last turn the first two
/// are long gone and only the newest stale one is left to clear.
#[tokio::test]
async fn media_is_counted_by_upstream() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.count_by_content.store(true, Relaxed);
    let image = json!({"type": "image",
        "source": {"type": "base64", "media_type": "image/png", "data": "AA"}});

    let (last, request) = run(&up, &base, 8, |_| json!(vec![image.clone(); 30])).await;

    assert_eq!(
        last["microcompact"],
        json!({"cleared_tool_results": 1, "tokens_freed": 30_000 - 33})
    );
    let results = tool_results(&request);
    for (id, content) in &results[..3] {
        assert_eq!(content, PLACEHOLDER, "{id} should be cleared");
    }
    for (id, content) in &results[3..] {
        assert_eq!(
            content.as_array().map(Vec::len),
            Some(30),
            "{id} should be kept"
        );
    }
}

/// If upstream cannot count the content, nothing is cleared: a guess is never
/// substituted, and the message itself still goes through.
#[tokio::test]
async fn nothing_is_cleared_when_upstream_cannot_count() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.count_by_content.store(true, Relaxed);
    up.count_fails.store(true, Relaxed);

    let (last, request) = run(&up, &base, 8, |_| json!("x".repeat(10_000))).await;

    for (id, content) in tool_results(&request) {
        assert_eq!(
            content.as_str().map(str::len),
            Some(10_000),
            "{id} must be intact"
        );
    }
    assert!(last.get("microcompact").is_none(), "{last}");
    assert_eq!(last["stop_reason"], "end_turn");
}
