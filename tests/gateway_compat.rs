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

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn bare_429() -> common::Scripted {
    common::Scripted {
        status: 429,
        headers: Vec::new(),
        body: json!({"type": "error", "error": {
            "type": "rate_limit_error",
            "message": "Usage credits are required for long context requests",
        }}),
    }
}

/// Bearer-authenticated endpoints answer in the Anthropic envelope,
/// `{"type":"error","error":{"type":...,"message":...}}`, so the SDKs can
/// surface the message.
#[tokio::test]
async fn errors_use_the_anthropic_envelope() {
    let (base, _proxy, _up) = start_with_gateway().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&json!({"max_tokens": 8}))
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 400);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["type"], "error", "{body}");
    assert!(body["error"]["type"].is_string(), "{body}");
    assert!(body["error"]["message"].is_string(), "{body}");
}

/// A 429 tells the caller when to come back (`Retry-After`) and that retrying
/// sooner is pointless (`x-should-retry: false`) -- both for the request that
/// hit upstream's limit and for the ones the gateway then refuses locally.
#[tokio::test]
async fn rate_limit_responses_say_when_to_retry() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.push(bare_429());
    let client = reqwest::Client::new();

    // 1. The request upstream rejected: no reset info -> the 60s fallback window.
    let first = client
        .post(format!("{base}/v1/messages"))
        .json(&messages_body())
        .send()
        .await
        .expect("request");
    assert_eq!(first.status(), 429);
    let retry_after: u64 = header(&first, "retry-after")
        .expect("retry-after on the passed-through 429")
        .parse()
        .expect("whole seconds");
    assert!((1..=60).contains(&retry_after), "retry-after {retry_after}");
    assert_eq!(header(&first, "x-should-retry").as_deref(), Some("false"));

    // 2. The next request is refused by the gateway itself, upstream untouched.
    let second = client
        .post(format!("{base}/v1/messages"))
        .json(&messages_body())
        .send()
        .await
        .expect("request");
    assert_eq!(second.status(), 429);
    let retry_after: u64 = header(&second, "retry-after")
        .expect("retry-after on a local refusal")
        .parse()
        .expect("whole seconds");
    assert!((1..=60).contains(&retry_after), "retry-after {retry_after}");
    assert_eq!(header(&second, "x-should-retry").as_deref(), Some("false"));
    let body: Value = second.json().await.expect("json body");
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(
        up.request_count(),
        1,
        "the second request must not reach upstream"
    );
}

/// When upstream does say when its limit resets, that is what the caller is told.
#[tokio::test]
async fn retry_after_follows_the_upstream_reset_time() {
    let (base, _proxy, up) = start_with_gateway().await;
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 3600;
    up.push(common::Scripted {
        status: 429,
        headers: vec![
            ("anthropic-ratelimit-unified-status", "rejected".to_string()),
            ("anthropic-ratelimit-unified-reset", reset.to_string()),
        ],
        body: json!({"type": "error", "error": {"type": "rate_limit_error", "message": "limit"}}),
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&messages_body())
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 429);
    let retry_after: u64 = header(&resp, "retry-after")
        .expect("retry-after")
        .parse()
        .expect("whole seconds");
    assert!(
        (3590..=3600).contains(&retry_after),
        "retry-after {retry_after}"
    );
}

/// `POST /v1/messages/count_tokens` is part of the protocol and is answered by
/// upstream. Only the fields that endpoint accepts are sent (a Messages body's
/// `max_tokens`, `temperature`, ... would be rejected there), and the caller's
/// model alias and `anthropic-beta` header are honored like on `/v1/messages`.
#[tokio::test]
async fn count_tokens_is_answered_by_upstream() {
    let (base, _proxy, up) = start_with_gateway().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages/count_tokens"))
        .header("anthropic-beta", "token-counting-2024-11-01")
        .json(&json!({
            "model": "sonnet",
            "max_tokens": 64,
            "temperature": 0,
            "stream": false,
            "system": "Be brief.",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["input_tokens"], 42);

    let sent = up.requests.lock().expect("lock")[0].clone();
    let mut fields: Vec<&str> = sent
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["messages", "model", "system"], "{sent}");
    assert_ne!(sent["model"], "sonnet", "the alias must be resolved");
    // No billing marker: the real request's usage does not count it either.
    assert_eq!(sent["system"], "Be brief.");
    assert!(
        up.last_header("anthropic-beta")
            .unwrap_or_default()
            .contains("token-counting-2024-11-01"),
        "{:?}",
        up.last_header("anthropic-beta")
    );
}

/// Without the direct API proxy there is nothing to count with: the protocol's
/// answer is `501 not_supported` + `x-should-retry: false`, after which the
/// client counts tokens itself.
#[tokio::test]
async fn count_tokens_answers_501_when_the_proxy_is_disabled() {
    let base = common::start_gateway_without_proxy().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages/count_tokens"))
        .json(&messages_body())
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 501);
    assert_eq!(header(&resp, "x-should-retry").as_deref(), Some("false"));
    let body: Value = resp.json().await.expect("json body");
    assert_eq!(body["type"], "error", "{body}");
    assert_eq!(body["error"]["type"], "not_supported", "{body}");
}

/// The session id the upstream request is attributed to (inside the JSON that
/// `metadata.user_id` carries).
fn upstream_session_id(up: &common::Upstream) -> String {
    let body = up
        .requests
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .expect("request");
    let user_id = body["metadata"]["user_id"]
        .as_str()
        .expect("metadata.user_id");
    let parsed: Value = serde_json::from_str(user_id).expect("user_id is JSON");
    parsed["session_id"]
        .as_str()
        .expect("session_id")
        .to_string()
}

/// Claude Code stamps every request with `X-Claude-Code-Session-Id`. Using it
/// as the upstream session keeps one conversation under one session id, which
/// is what the prompt cache is keyed on. `X-Session-Id` (the gateway's own
/// header) still wins when both are present, and with neither every request
/// gets a fresh id as before.
#[tokio::test]
async fn claude_code_session_id_header_becomes_the_upstream_session() {
    let (base, _proxy, up) = start_with_gateway().await;
    let client = reqwest::Client::new();
    let send = |headers: Vec<(&'static str, &'static str)>| {
        let client = client.clone();
        let url = format!("{base}/v1/messages");
        async move {
            let mut req = client.post(url).json(&messages_body());
            for (k, v) in headers {
                req = req.header(k, v);
            }
            assert_eq!(req.send().await.expect("request").status(), 200);
        }
    };

    send(vec![("x-claude-code-session-id", "sess-from-claude-code")]).await;
    assert_eq!(upstream_session_id(&up), "sess-from-claude-code");

    send(vec![
        ("x-session-id", "sess-explicit"),
        ("x-claude-code-session-id", "sess-from-claude-code"),
    ])
    .await;
    assert_eq!(upstream_session_id(&up), "sess-explicit");

    send(vec![]).await;
    let first = upstream_session_id(&up);
    send(vec![]).await;
    assert_ne!(
        first,
        upstream_session_id(&up),
        "no header -> a fresh id per request"
    );
}

/// What the upstream sees is the client identity the CLI 2.1.314 presents:
/// its User-Agent, SDK version, session header, and a billing block whose
/// `cc_version` fingerprint follows the conversation.
#[tokio::test]
async fn upstream_sees_the_current_client_identity() {
    let (base, _proxy, up) = start_with_gateway().await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-session-id", "sess-identity")
        .json(&json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Reply with the single word: ok"}],
        }))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 200);

    assert!(
        up.last_header("user-agent")
            .unwrap_or_default()
            .starts_with("claude-cli/2.1.314 ("),
        "{:?}",
        up.last_header("user-agent")
    );
    assert_eq!(
        up.last_header("x-stainless-package-version").as_deref(),
        Some("0.128.0")
    );
    // The header and metadata.user_id name the same session.
    assert_eq!(
        up.last_header("x-claude-code-session-id").as_deref(),
        Some("sess-identity")
    );
    assert_eq!(upstream_session_id(&up), "sess-identity");

    let body = up
        .requests
        .lock()
        .expect("lock")
        .last()
        .cloned()
        .expect("request");
    let first_system = body["system"][0]["text"].as_str().expect("system[0].text");
    assert!(
        first_system.starts_with("x-anthropic-billing-header: cc_version=2.1.314.d8d;"),
        "{first_system}"
    );
}

/// `GET /v1/models` and `GET /v1/models/{id}` (optional in the protocol, and
/// what the SDKs' `models.list()` / `models.retrieve()` call) are answered by
/// upstream, query string included.
#[tokio::test]
async fn models_endpoints_are_answered_by_upstream() {
    let (base, _proxy, up) = start_with_gateway().await;
    let client = reqwest::Client::new();

    let list: Value = client
        .get(format!("{base}/v1/models?limit=5"))
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("json");
    assert_eq!(list["data"][0]["id"], "claude-sonnet-5-5", "{list}");

    let one: Value = client
        .get(format!("{base}/v1/models/claude-opus-4-6"))
        .send()
        .await
        .expect("retrieve")
        .json()
        .await
        .expect("json");
    assert_eq!(one["id"], "claude-opus-4-6", "{one}");
    assert_eq!(one["max_input_tokens"], 1_000_000);

    assert_eq!(
        *up.get_requests.lock().expect("lock"),
        ["/v1/models?limit=5", "/v1/models/claude-opus-4-6"]
    );
}

/// The id ends up in the upstream URL, so anything that is not a model id is
/// refused before a request is made.
#[tokio::test]
async fn model_ids_that_are_not_ids_never_reach_upstream() {
    let (base, _proxy, up) = start_with_gateway().await;
    let client = reqwest::Client::new();

    for bad in ["..%2F..%2Foauth%2Ftoken", "a%3Fb%3Dc", "a%23b", "a%20b"] {
        let resp = client
            .get(format!("{base}/v1/models/{bad}"))
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), 400, "{bad}");
        let body: Value = resp.json().await.expect("json");
        assert_eq!(body["type"], "error", "{bad}: {body}");
    }
    assert!(up.get_requests.lock().expect("lock").is_empty());
}

/// Without the direct API proxy there is no model list to give: `404`, which
/// the protocol defines as "use your built-in list".
#[tokio::test]
async fn models_answer_404_when_the_proxy_is_disabled() {
    let base = common::start_gateway_without_proxy().await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .expect("request");
    assert_eq!(resp.status(), 404);
}

fn overflow_400(input: u64, max_tokens: u64, limit: u64) -> common::Scripted {
    common::Scripted {
        status: 400,
        headers: Vec::new(),
        body: json!({"type": "error", "error": {
            "type": "invalid_request_error",
            "message": format!(
                "input length and `max_tokens` exceed context limit: {input} + {max_tokens} > {limit}, \
                 decrease input length or `max_tokens` and try again"
            ),
        }}),
    }
}

fn upstream_max_tokens(up: &common::Upstream) -> Vec<u64> {
    up.requests
        .lock()
        .expect("lock")
        .iter()
        .map(|r| r["max_tokens"].as_u64().unwrap_or(0))
        .collect()
}

/// Token counts cannot be known locally, but the server states them when a
/// request does not fit: `input + max_tokens > limit`. Claude Code reads those
/// numbers and retries with `max_tokens = limit - input - 1000`; so does the
/// gateway, instead of handing the caller a 400 it could not have avoided.
#[tokio::test]
async fn max_tokens_that_does_not_fit_is_lowered_from_the_servers_numbers() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.push(overflow_400(990_000, 64_000, 1_000_000));
    let mut body = messages_body();
    body["max_tokens"] = json!(64_000);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    // 1_000_000 - 990_000 - 1_000
    assert_eq!(upstream_max_tokens(&up), [64_000, 9_000]);
}

#[tokio::test]
async fn the_streaming_path_lowers_max_tokens_the_same_way() {
    let (base, _proxy, up) = start_with_gateway().await;
    up.push(overflow_400(990_000, 64_000, 1_000_000));
    let mut body = messages_body();
    body["max_tokens"] = json!(64_000);
    body["stream"] = json!(true);

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .json(&body)
        .send()
        .await
        .expect("request");

    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("text/event-stream")
    );
    let text = resp.text().await.expect("body");
    assert!(text.contains("event: message_stop"), "{text:?}");
    assert_eq!(upstream_max_tokens(&up), [64_000, 9_000]);
}

/// Less than 3,000 tokens left for the answer (the CLI's FLOOR_OUTPUT_TOKENS),
/// or a request that is already at or below the adjusted value, is not retried:
/// the caller gets upstream's own 400.
#[tokio::test]
async fn overflow_errors_that_cannot_be_fixed_by_lowering_max_tokens_pass_through() {
    for (label, input, max_tokens, limit) in [
        (
            "only 500 tokens would be left",
            998_500_u64,
            64_000_u64,
            1_000_000_u64,
        ),
        // 5005 - 10 - 1000 = 3995, but the request already asks for just 3000.
        ("no progress possible", 10, 3_000, 5_005),
    ] {
        let (base, _proxy, up) = start_with_gateway().await;
        up.push(overflow_400(input, max_tokens, limit));
        let mut body = messages_body();
        body["max_tokens"] = json!(max_tokens);

        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .json(&body)
            .send()
            .await
            .expect("request");

        assert_eq!(resp.status(), 400, "{label}");
        let text = resp.text().await.expect("body");
        assert!(text.contains("exceed context limit"), "{label}: {text}");
        assert_eq!(up.request_count(), 1, "{label}: must not retry");
    }
}
