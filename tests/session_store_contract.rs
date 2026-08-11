//! Contract tests for the per-provider session stores.
//!
//! The Claude, Codex and Codex app-server stores are independent types that
//! are expected to behave identically. These tests pin that shared behaviour
//! so it can be verified before and after the stores are merged behind a
//! single generic implementation (see `docs/PHASE2_COMMON_LAYER.md`).
//!
//! Every case runs against all three stores via `store_contract!`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex};

use claude_agent::codex::options::CodexOptions;
use claude_agent::codex::session::{CodexSession, CodexSessionState};
use claude_agent::codex::store::CodexSessionStore;
use claude_agent::codex_app::session::{CodexAppSession, CodexAppSessionState};
use claude_agent::codex_app::store::CodexAppSessionStore;
use claude_agent::options::ClaudeAgentOptions;
use claude_agent::session::store::SessionStore;
use claude_agent::session::{Session, SessionState};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Session constructors ────────────────────────────────────────────
//
// `activity_ms` is the epoch-millis stamp the cleanup pass compares against;
// `dead` selects the provider's terminal state.

fn claude_session(id: &str, activity_ms: u64, dead: bool) -> Arc<Session> {
    let (stdin_tx, _stdin_rx) = mpsc::channel::<String>(1);
    let (event_tx, _) = broadcast::channel(1);
    Arc::new(Session {
        id: id.to_string(),
        cli_session_id: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(if dead {
            SessionState::Dead
        } else {
            SessionState::Idle
        })),
        created_at: std::time::Instant::now(),
        last_activity_ms: AtomicU64::new(activity_ms),
        options: ClaudeAgentOptions::default(),
        stdin_tx,
        event_tx,
        history: Arc::new(Mutex::new(VecDeque::new())),
        hook_timeout_handle: Arc::new(Mutex::new(None)),
        next_seq: AtomicU64::new(0),
    })
}

fn codex_session(id: &str, activity_ms: u64, dead: bool) -> Arc<CodexSession> {
    let (event_tx, _) = broadcast::channel(1);
    Arc::new(CodexSession {
        id: id.to_string(),
        thread_id: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(if dead {
            CodexSessionState::Dead
        } else {
            CodexSessionState::Idle
        })),
        created_at: std::time::Instant::now(),
        last_activity_ms: AtomicU64::new(activity_ms),
        options: CodexOptions::default(),
        event_tx,
        history: Arc::new(Mutex::new(VecDeque::new())),
        next_seq: AtomicU64::new(0),
    })
}

fn codex_app_session(id: &str, activity_ms: u64, dead: bool) -> Arc<CodexAppSession> {
    let (stdin_tx, _stdin_rx) = mpsc::channel::<String>(1);
    let (event_tx, _) = broadcast::channel(1);
    Arc::new(CodexAppSession {
        id: id.to_string(),
        thread_id: Arc::new(Mutex::new(None)),
        turn_id: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(if dead {
            CodexAppSessionState::Dead
        } else {
            CodexAppSessionState::Idle
        })),
        created_at: std::time::Instant::now(),
        last_activity_ms: AtomicU64::new(activity_ms),
        options: CodexOptions::default(),
        stdin_tx,
        event_tx,
        history: Arc::new(Mutex::new(VecDeque::new())),
        next_seq: AtomicU64::new(0),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        pending_approval: Arc::new(Mutex::new(None)),
        next_request_id: AtomicU64::new(0),
    })
}

/// Generate the full contract suite for one store type.
///
/// `$store` is the store constructor, `$make` builds a session
/// `(id, activity_ms, dead) -> Arc<S>`.
macro_rules! store_contract {
    ($modname:ident, $store:ty, $make:ident) => {
        mod $modname {
            use super::*;

            #[tokio::test]
            async fn get_returns_the_inserted_session() {
                let store = <$store>::new(4);
                store.insert($make("a", now_ms(), false)).unwrap();

                assert_eq!(store.get("a").unwrap().id, "a");
            }

            #[tokio::test]
            async fn get_reports_not_found_for_an_unknown_id() {
                let store = <$store>::new(4);

                let err = store.get("nope").err().expect("expected a not-found error");
                assert!(
                    err.to_string().to_lowercase().contains("not found"),
                    "unexpected error: {err}"
                );
            }

            #[tokio::test]
            async fn insert_enforces_the_session_limit() {
                let store = <$store>::new(1);
                store.insert($make("a", now_ms(), false)).unwrap();

                let err = store
                    .insert($make("b", now_ms(), false))
                    .err()
                    .expect("expected a session-limit error");
                assert!(
                    err.to_string().contains("Concurrent session limit reached"),
                    "unexpected error: {err}"
                );
                assert_eq!(store.list().len(), 1);
            }

            /// The limit is evaluated live, so freeing a slot admits a new session.
            #[tokio::test]
            async fn removing_a_session_frees_a_slot() {
                let store = <$store>::new(1);
                store.insert($make("a", now_ms(), false)).unwrap();
                assert!(store.remove("a"));

                store.insert($make("b", now_ms(), false)).unwrap();
                assert_eq!(store.list().len(), 1);
            }

            #[tokio::test]
            async fn remove_reports_whether_it_removed_anything() {
                let store = <$store>::new(4);
                store.insert($make("a", now_ms(), false)).unwrap();

                assert!(store.remove("a"));
                assert!(!store.remove("a"));
            }

            #[tokio::test]
            async fn list_returns_every_live_session() {
                let store = <$store>::new(4);
                store.insert($make("a", now_ms(), false)).unwrap();
                store.insert($make("b", now_ms(), false)).unwrap();

                let mut ids: Vec<String> = store.list().iter().map(|s| s.id.clone()).collect();
                ids.sort();
                assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
            }

            /// Re-inserting the same id replaces rather than duplicating, and
            /// must not consume a second slot against the limit.
            #[tokio::test]
            async fn inserting_a_duplicate_id_replaces_in_place() {
                let store = <$store>::new(2);
                store.insert($make("a", now_ms(), false)).unwrap();
                store.insert($make("a", now_ms(), false)).unwrap();

                assert_eq!(store.list().len(), 1);
            }

            #[tokio::test]
            async fn cleanup_removes_sessions_idle_past_the_timeout() {
                let store = <$store>::new(4);
                // Epoch 0 is unreachably long ago, so any timeout expires it.
                store.insert($make("stale", 0, false)).unwrap();
                store.insert($make("fresh", now_ms(), false)).unwrap();

                store.run_cleanup(60).await;

                let ids: Vec<String> = store.list().iter().map(|s| s.id.clone()).collect();
                assert_eq!(ids, vec!["fresh".to_string()]);
            }

            /// A dead session is collected even when it was just active — the
            /// subprocess behind it is gone, so idleness is irrelevant.
            #[tokio::test]
            async fn cleanup_removes_dead_sessions_regardless_of_activity() {
                let store = <$store>::new(4);
                store.insert($make("dead", now_ms(), true)).unwrap();

                store.run_cleanup(3600).await;

                assert!(store.list().is_empty());
            }

            #[tokio::test]
            async fn cleanup_keeps_live_sessions() {
                let store = <$store>::new(4);
                store.insert($make("live", now_ms(), false)).unwrap();

                store.run_cleanup(3600).await;

                assert_eq!(store.list().len(), 1);
            }

            #[tokio::test]
            async fn cleanup_is_a_no_op_on_an_empty_store() {
                let store = <$store>::new(4);

                store.run_cleanup(0).await;

                assert!(store.list().is_empty());
            }

            /// The store's own lock must make the len-check and the insert
            /// atomic; without it, racing inserts overshoot `max_sessions`.
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn concurrent_inserts_never_exceed_the_limit() {
                const MAX: usize = 8;
                const ATTEMPTS: usize = 64;

                let store = Arc::new(<$store>::new(MAX));
                let mut tasks = Vec::new();

                for i in 0..ATTEMPTS {
                    let store = store.clone();
                    tasks.push(tokio::spawn(async move {
                        store
                            .insert($make(&format!("s{i}"), now_ms(), false))
                            .is_ok()
                    }));
                }

                let mut accepted = 0;
                for task in tasks {
                    if task.await.unwrap() {
                        accepted += 1;
                    }
                }

                assert_eq!(accepted, MAX, "accepted more sessions than the limit");
                assert_eq!(store.list().len(), MAX);
            }

            /// Cleanup must tolerate concurrent traffic against the store.
            /// This is the shape that a shard guard held across an await would
            /// stall on.
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn cleanup_runs_concurrently_with_inserts_and_removes() {
                let store = Arc::new(<$store>::new(512));
                for i in 0..64 {
                    let _ = store.insert($make(&format!("seed{i}"), 0, false));
                }

                let writer = {
                    let store = store.clone();
                    tokio::spawn(async move {
                        for i in 0..256 {
                            let _ = store.insert($make(&format!("w{i}"), now_ms(), false));
                            store.remove(&format!("w{i}"));
                            tokio::task::yield_now().await;
                        }
                    })
                };

                let cleaner = {
                    let store = store.clone();
                    tokio::spawn(async move {
                        for _ in 0..32 {
                            store.run_cleanup(60).await;
                            tokio::task::yield_now().await;
                        }
                    })
                };

                let joined = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    tokio::try_join!(writer, cleaner)
                })
                .await;

                assert!(
                    joined.is_ok(),
                    "cleanup and mutation deadlocked or starved each other"
                );
                joined.unwrap().unwrap();

                // Every seeded session was stale, so cleanup collected them all.
                assert!(store.list().is_empty());
            }
        }
    };
}

store_contract!(claude, SessionStore, claude_session);
store_contract!(codex, CodexSessionStore, codex_session);
store_contract!(codex_app, CodexAppSessionStore, codex_app_session);
