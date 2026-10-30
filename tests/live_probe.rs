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

/// Does subscription traffic get through to upstream `count_tokens` and to the
/// models endpoint, and what context window does upstream report for the 4.6
/// models? (count_tokens and the models listing cost nothing.)
#[tokio::test]
#[ignore = "calls the real Anthropic API with the developer's credentials"]
async fn count_tokens_and_model_metadata_on_oauth_traffic() {
    let state = Arc::new(ProxyState::new(1));
    let messages = json!([{"role": "user", "content": "Reply with the single word: ok"}]);

    for model in ["claude-sonnet-5-5", "claude-haiku-5-5", "claude-opus-5-5"] {
        let body = json!({"model": model, "max_tokens": 64, "temperature": 0, "stream": false,
                          "messages": messages});
        let plain = proxy::count_tokens(&state, &body, None, "live-probe").await;
        summarize_count(&format!("count_tokens {model} (no billing block)"), plain);

        let mut with_block = body.clone();
        claude_agent::client_identity::inject_billing_system_block(&mut with_block);
        let blocked = proxy::count_tokens(&state, &with_block, None, "live-probe").await;
        summarize_count(
            &format!("count_tokens {model} (with billing block)"),
            blocked,
        );
    }

    for id in [
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-sonnet-5-5",
        "claude-opus-4-5",
    ] {
        match proxy::upstream_get(&state, &format!("/v1/models/{id}")).await {
            Ok((m, 200)) => eprintln!(
                "[probe] model {id}: max_input_tokens={} max_tokens={} display={}",
                m["max_input_tokens"], m["max_tokens"], m["display_name"]
            ),
            Ok((m, status)) => eprintln!(
                "[probe] model {id}: HTTP {status} {}",
                m["error"]["message"]
            ),
            Err(e) => eprintln!("[probe] model {id}: gateway error: {e}"),
        }
    }
}

fn summarize_count(label: &str, result: Result<(Value, u16), proxy::ProxyError>) {
    match result {
        Ok((body, 200)) => eprintln!("[probe] {label}: 200 input_tokens={}", body["input_tokens"]),
        Ok((body, status)) => {
            eprintln!(
                "[probe] {label}: HTTP {status} {}",
                body["error"]["message"]
            )
        }
        Err(e) => eprintln!("[probe] {label}: gateway error: {e}"),
    }
}

/// Every model in the gateway's catalog against what upstream reports for it
/// (`GET /v1/models/{id}`: `max_input_tokens`, `max_tokens`). Run it when the
/// catalog is edited or a new model appears; a model upstream no longer serves
/// shows up as a 404 line rather than a failure.
#[tokio::test]
#[ignore = "calls the real Anthropic API with the developer's credentials"]
async fn model_catalog_matches_upstream_limits() {
    let state = Arc::new(ProxyState::new(1));
    let mut mismatches = Vec::new();

    for model in claude_agent::models::ALL_MODELS {
        match proxy::upstream_get(&state, &format!("/v1/models/{}", model.id)).await {
            Ok((m, 200)) => {
                let window = m["max_input_tokens"].as_u64();
                let output = m["max_tokens"].as_u64();
                let ok = window == Some(u64::from(model.context_window))
                    && output == Some(u64::from(model.max_output_tokens));
                eprintln!(
                    "[probe] {:<20} {} window gateway={} upstream={:?} | max_output gateway={} upstream={:?}",
                    model.id,
                    if ok { "ok      " } else { "MISMATCH" },
                    model.context_window,
                    window,
                    model.max_output_tokens,
                    output
                );
                if !ok {
                    mismatches.push(model.id);
                }
            }
            Ok((m, status)) => eprintln!(
                "[probe] {:<20} HTTP {status} {}",
                model.id, m["error"]["message"]
            ),
            Err(e) => eprintln!("[probe] {:<20} gateway error: {e}", model.id),
        }
    }
    eprintln!("[probe] mismatches: {mismatches:?}");
}

/// Which models reject a request whose `input + max_tokens` exceeds the window,
/// and does the retry with the lowered `max_tokens` then go through? Models with
/// a 200k window are used so that about 150k tokens of input plus `max_tokens`
/// 64000 cannot fit. The input size is tuned with `count_tokens` (free); every
/// request that is accepted is billed for that input.
///
/// "upstream attempts" is `ProxyState::total_requests`, which counts each
/// upstream attempt of `messages_sync`: 1 means upstream took the request as it
/// was, 2 means it was rejected for not fitting and the lowered retry succeeded.
#[tokio::test]
#[ignore = "calls the real Anthropic API; bills ~150k input tokens per accepted request"]
async fn overflowing_max_tokens_is_lowered_and_the_request_succeeds() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    for model in ["claude-sonnet-4-5", "claude-opus-4-5"] {
        let state = Arc::new(ProxyState::new(1));
        let build = |repeats: usize| -> Value {
            let filler: String = (0..repeats)
                .map(|i| {
                    format!("Entry {i}: the quick brown fox number {i} jumps over lazy dog {i}. ")
                })
                .collect();
            json!({
                "model": model,
                "max_tokens": 64_000,
                "messages": [{"role": "user", "content": format!("{filler}\n\nReply with the single word: ok")}],
            })
        };

        // Tune the input to ~150k tokens by counting, not guessing.
        let mut repeats = 6_600usize;
        let mut tokens = 0u64;
        for _ in 0..4 {
            let (counted, status) =
                proxy::count_tokens(&state, &build(repeats), None, "live-probe")
                    .await
                    .expect("count_tokens");
            assert_eq!(status, 200, "{counted}");
            tokens = counted["input_tokens"].as_u64().expect("input_tokens");
            if (140_000..=170_000).contains(&tokens) {
                break;
            }
            repeats = (repeats as f64 * 150_000.0 / tokens as f64) as usize;
        }
        assert!(
            (140_000..=170_000).contains(&tokens),
            "could not tune the input: {tokens}"
        );

        let (resp, status) = proxy::messages_sync(&state, build(repeats), None, "live-probe")
            .await
            .expect("messages_sync");
        let attempts = state
            .total_requests
            .load(std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[probe] {model}: counted {tokens} input tokens + max_tokens 64000 -> status={status} \
             upstream attempts={attempts} stop_reason={} error={}",
            resp["stop_reason"], resp["error"]["message"]
        );
    }
}

/// Does upstream accept the two `count_tokens` requests microcompact relies on
/// (the old tool-result content as plain user blocks, and the placeholders that
/// replace it), and do their counts subtract to what was freed? Free to run.
#[tokio::test]
#[ignore = "calls the real Anthropic API with the developer's credentials"]
async fn microcompact_count_requests_are_accepted_upstream() {
    use claude_agent::proxy_session::{MicrocompactOptions, ProxySession, SessionOptions};

    let state = Arc::new(ProxyState::new(1));
    let mut session = ProxySession::new(
        "live-probe".to_string(),
        SessionOptions {
            model: Some("claude-sonnet-5-5".to_string()),
            microcompact: Some(MicrocompactOptions {
                trigger_tokens: 1,
                compactable_tools: None,
            }),
            ..Default::default()
        },
        3600,
    );
    // 1x1 PNG.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    for i in 0..9 {
        let id = format!("toolu_{i}");
        session
            .messages
            .push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Read", "input": {"path": "/x"}}]}));
        let content = if i == 1 {
            json!([{"type": "text", "text": "a picture"},
                   {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}])
        } else {
            json!("The quick brown fox jumps over the lazy dog. ".repeat(250))
        };
        session.add_tool_result(vec![
            json!({"type": "tool_result", "tool_use_id": id, "content": content}),
        ]);
    }
    session.last_input_tokens = 150_000;

    let plan = session.plan_microcompact().expect("a plan");
    let mut counts = Vec::new();
    for (label, body) in [
        ("before", plan.count_request_before()),
        ("after", plan.count_request_after()),
    ] {
        let (reply, status) = proxy::count_tokens(&state, &body, None, "live-probe")
            .await
            .expect("count_tokens");
        eprintln!("[probe] {label}: HTTP {status} {reply}");
        assert_eq!(status, 200, "{label}: {reply}");
        counts.push(reply["input_tokens"].as_u64().expect("input_tokens"));
    }
    eprintln!(
        "[probe] freed = {} - {} = {}",
        counts[0],
        counts[1],
        counts[0].saturating_sub(counts[1])
    );
    assert!(counts[0] > counts[1]);
}
