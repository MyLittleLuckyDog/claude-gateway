//! How the direct API proxy starts: `claude_agent::proxy::start`, which the
//! server calls from `main`. It once lost its startup quota check to an edit of
//! the surrounding code and no test noticed.

mod common;

use claude_agent::models::ModelFamily;
use claude_agent::proxy;

/// With a usable token the proxy starts, and the startup quota check goes out:
/// one minimal Haiku request, whose answer fills the rate-limit cache.
#[tokio::test]
async fn startup_runs_the_quota_check() {
    let (base, up) = common::start_stub().await;

    let started = proxy::start(true, 2, Some(&base)).await;

    let state = started.state.expect("the proxy starts with credentials");
    assert_eq!(started.unavailable, None);
    assert_eq!(
        state.semaphore.available_permits(),
        2,
        "max_concurrent is honored"
    );

    assert_eq!(up.request_count(), 1, "exactly one quota check");
    let sent = up.requests.lock().expect("lock")[0].clone();
    assert_eq!(sent["max_tokens"], 1, "{sent}");
    assert_eq!(sent["model"], "claude-haiku-4-5", "{sent}");
    assert!(
        state
            .rate_limit
            .read()
            .await
            .contains_key(&ModelFamily::Haiku),
        "the check's answer must land in the rate-limit cache"
    );
}

/// Switched off in the configuration: no proxy, no request, and the reason says so.
#[tokio::test]
async fn startup_reports_a_proxy_switched_off_in_the_configuration() {
    let (base, up) = common::start_stub().await;

    let started = proxy::start(false, 2, Some(&base)).await;

    assert!(started.state.is_none());
    let reason = started.unavailable.expect("a reason");
    assert!(reason.contains("proxy.enabled = false"), "{reason}");
    assert_eq!(up.request_count(), 0, "nothing is sent when it is off");
}
