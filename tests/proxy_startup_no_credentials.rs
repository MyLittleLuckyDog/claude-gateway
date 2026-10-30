//! The proxy cannot start without a usable OAuth token, and says why. This is its
//! own test binary because it points the process at an empty credentials
//! directory, which would break every other test sharing the process.

use claude_agent::proxy;

#[tokio::test]
async fn startup_without_credentials_reports_why() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".claude")).expect("config dir");
    std::env::set_var("HOME", dir.path());
    std::env::set_var("CLAUDE_CONFIG_DIR", dir.path().join(".claude"));
    std::env::set_var("CLAUDE_GATEWAY_SKIP_KEYCHAIN", "1");

    let started = proxy::start(true, 2, Some("http://127.0.0.1:1")).await;

    assert!(started.state.is_none());
    let reason = started.unavailable.expect("a reason");
    assert!(
        reason.contains("no usable Claude OAuth credentials"),
        "{reason}"
    );
    // The underlying cause is in it, not just a generic sentence.
    assert!(reason.contains("claude /login"), "{reason}");
}
