//! Rate-limit cache behaviour of the direct API proxy, against the local
//! upstream stub.

mod common;

use claude_agent::models::ModelFamily;
use claude_agent::proxy::{self, ProxyError, ProxyState, REJECTION_FALLBACK_TTL_SECS};
use common::{ok_reply, start, user_turns, Scripted};
use serde_json::{json, Value};

fn request() -> Value {
    json!({
        "model": "claude-sonnet-5-5",
        "max_tokens": 64,
        "messages": user_turns(1),
    })
}

fn bare_429() -> Scripted {
    Scripted {
        status: 429,
        headers: Vec::new(),
        body: json!({"type": "error", "error": {
            "type": "rate_limit_error",
            "message": "Usage credits are required for long context requests",
        }}),
    }
}

/// Precondition of the lock-up: a 429 that carries no reset information leaves
/// the family `rejected` with nothing to expire it, and the next request is
/// refused locally without ever reaching upstream.
#[tokio::test]
async fn bare_429_leaves_family_rejected_and_blocks_next_request_locally() {
    let (state, up) = start().await;
    up.push(bare_429());

    let (_, status) = proxy::messages_sync(&state, request(), None, "s1")
        .await
        .expect("first call");
    assert_eq!(status, 429);

    {
        let map = state.rate_limit.read().await;
        let rl = map.get(&ModelFamily::Sonnet).expect("sonnet entry");
        assert_eq!(rl.status.as_deref(), Some("rejected"));
        assert_eq!(rl.resets_at, None);
    }

    let err = proxy::messages_sync(&state, request(), None, "s1")
        .await
        .expect_err("second call must be refused locally");
    assert!(matches!(err, ProxyError::RateLimited { .. }));
    assert_eq!(up.request_count(), 1, "second call must not reach upstream");
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Pretend `secs` seconds have passed since the family was rejected.
async fn age_rejection(state: &ProxyState, secs: f64) {
    let mut map = state.rate_limit.write().await;
    let rl = map.get_mut(&ModelFamily::Sonnet).expect("sonnet entry");
    rl.rejected_at = rl.rejected_at.map(|t| t - secs);
    rl.resets_at = rl.resets_at.map(|t| t - secs);
}

async fn call(state: &std::sync::Arc<ProxyState>) -> Result<u16, ProxyError> {
    proxy::messages_sync(state, request(), None, "s1")
        .await
        .map(|(_, status)| status)
}

/// The fix: a rejection with no reset information expires on its own, and the
/// request that follows reaches upstream and succeeds.
#[tokio::test]
async fn bare_429_rejection_expires_and_upstream_is_probed_again() {
    let (state, up) = start().await;
    up.push(bare_429());
    assert_eq!(call(&state).await.expect("first"), 429);

    // Still inside the TTL: refused locally, upstream untouched.
    assert!(call(&state).await.is_err());
    assert_eq!(up.request_count(), 1);

    // Past the TTL: let through, and the stub's default reply is a 200.
    age_rejection(&state, REJECTION_FALLBACK_TTL_SECS + 1.0).await;
    assert_eq!(call(&state).await.expect("after ttl"), 200);
    assert_eq!(up.request_count(), 2);

    // Healthy again: the cached state no longer blocks.
    assert_eq!(call(&state).await.expect("steady state"), 200);
}

/// A probe that is rejected again re-arms the TTL rather than unblocking for good.
#[tokio::test]
async fn probe_that_is_rejected_again_blocks_again() {
    let (state, up) = start().await;
    up.push(bare_429());
    up.push(bare_429());
    assert_eq!(call(&state).await.expect("first"), 429);
    age_rejection(&state, REJECTION_FALLBACK_TTL_SECS + 1.0).await;

    assert_eq!(call(&state).await.expect("probe"), 429);
    assert!(
        call(&state).await.is_err(),
        "blocked again after a failed probe"
    );
    assert_eq!(up.request_count(), 2);
}

/// When upstream does say when the limit resets, that time is honoured —
/// the short fallback TTL must not cut a real multi-hour rejection short.
#[tokio::test]
async fn reset_header_is_honoured_over_the_fallback_ttl() {
    let (state, up) = start().await;
    let reset = now() + 3600.0;
    up.push(Scripted {
        status: 429,
        headers: vec![
            ("anthropic-ratelimit-unified-status", "rejected".to_string()),
            ("anthropic-ratelimit-unified-reset", format!("{reset}")),
        ],
        body: json!({"type": "error", "error": {"type": "rate_limit_error", "message": "limit"}}),
    });
    assert_eq!(call(&state).await.expect("first"), 429);

    // Well past the fallback TTL but before the real reset: still blocked.
    age_rejection(&state, REJECTION_FALLBACK_TTL_SECS + 600.0).await;
    assert!(call(&state).await.is_err());
    assert_eq!(up.request_count(), 1);

    // Past the real reset: allowed.
    age_rejection(&state, 3600.0).await;
    assert_eq!(call(&state).await.expect("after reset"), 200);
}

/// `retry-after` on a 429 bounds the rejection when no reset header is present.
#[tokio::test]
async fn retry_after_sets_the_expiry() {
    let (state, up) = start().await;
    up.push(Scripted {
        status: 429,
        headers: vec![("retry-after", "120".to_string())],
        body: json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}}),
    });
    assert_eq!(call(&state).await.expect("first"), 429);

    let resets_at = state
        .rate_limit
        .read()
        .await
        .get(&ModelFamily::Sonnet)
        .and_then(|rl| rl.resets_at)
        .expect("resets_at from retry-after");
    let delta = resets_at - now();
    assert!((100.0..=125.0).contains(&delta), "expiry {delta}s away");
}

/// A reset remembered from an earlier successful response describes a
/// different window; a later bare 429 must not inherit it.
#[tokio::test]
async fn stale_reset_from_an_earlier_response_does_not_stretch_a_bare_429() {
    let (state, up) = start().await;
    up.push(Scripted {
        status: 200,
        headers: vec![
            ("anthropic-ratelimit-unified-status", "allowed".to_string()),
            (
                "anthropic-ratelimit-unified-reset",
                format!("{}", now() + 7.0 * 86_400.0),
            ),
        ],
        body: ok_reply("claude-sonnet-5-5", 10),
    });
    assert_eq!(call(&state).await.expect("first"), 200);

    up.push(bare_429());
    assert_eq!(call(&state).await.expect("second"), 429);
    assert_eq!(
        state
            .rate_limit
            .read()
            .await
            .get(&ModelFamily::Sonnet)
            .and_then(|rl| rl.resets_at),
        None,
        "the week-old reset must not carry over"
    );

    age_rejection(&state, REJECTION_FALLBACK_TTL_SECS + 1.0).await;
    assert_eq!(call(&state).await.expect("after ttl"), 200);
}
