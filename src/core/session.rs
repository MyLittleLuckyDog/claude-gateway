//! Provider-agnostic session registry.
//!
//! Each provider axis owns its own session struct and state machine; the only
//! thing they share is how sessions are held, looked up, capped and reaped.
//! [`ManagedSession`] is the narrow slice of a session that this registry
//! needs, and deliberately nothing more — a provider's waiting states
//! (`WaitingForHook`, `WaitingForApproval`, …) stay private to that provider.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::core::now_epoch_ms;
use crate::error::GatewayError;

/// A session that [`SessionStore`] can hold, cap and reap.
#[async_trait]
pub trait ManagedSession: Send + Sync + 'static {
    /// Key this session is stored under.
    fn id(&self) -> &str;

    /// Epoch millis of the last activity on this session.
    fn last_activity_ms(&self) -> u64;

    /// Human-readable provider label, used only in cleanup logs.
    fn kind() -> &'static str
    where
        Self: Sized;

    /// Whether the session has reached its provider's terminal state.
    ///
    /// Terminal sessions are reaped regardless of how recently they were
    /// active — the process behind them is already gone.
    async fn is_terminal(&self) -> bool;
}

/// Bounded registry of live sessions for one provider axis.
pub struct SessionStore<S: ManagedSession> {
    sessions: DashMap<String, Arc<S>>,
    max_sessions: usize,
    /// Makes the capacity check and the insert atomic with respect to each
    /// other. `DashMap` alone would let racing inserts both observe
    /// `len() < max` and overshoot the cap.
    op_lock: std::sync::Mutex<()>,
}

impl<S: ManagedSession> SessionStore<S> {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: DashMap::new(),
            max_sessions,
            op_lock: std::sync::Mutex::new(()),
        }
    }

    /// Register `session`, rejecting it once the store is at capacity.
    ///
    /// The capacity check is on occupancy alone, so at capacity even a session
    /// whose id is already present is rejected rather than replaced. Session
    /// ids are freshly generated UUIDs and are never re-inserted, so this is
    /// unreachable in practice; it is kept as-is to preserve the behaviour of
    /// the three stores this replaced.
    pub fn insert(&self, session: Arc<S>) -> Result<(), GatewayError> {
        let _guard = self.op_lock.lock().unwrap();
        if self.sessions.len() >= self.max_sessions {
            return Err(GatewayError::SessionLimitReached {
                max: self.max_sessions,
            });
        }
        self.sessions.insert(session.id().to_string(), session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Arc<S>, GatewayError> {
        self.sessions
            .get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| GatewayError::SessionNotFound(id.to_string()))
    }

    pub fn remove(&self, id: &str) -> bool {
        let _guard = self.op_lock.lock().unwrap();
        self.sessions.remove(id).is_some()
    }

    pub fn count(&self) -> usize {
        self.sessions.len()
    }

    pub fn list(&self) -> Vec<Arc<S>> {
        self.sessions.iter().map(|r| r.value().clone()).collect()
    }

    /// Reap sessions that are terminal or have been idle past
    /// `idle_timeout_secs`.
    pub async fn run_cleanup(&self, idle_timeout_secs: u64) {
        let now_ms = now_epoch_ms();
        let timeout_ms = idle_timeout_secs.saturating_mul(1000);

        // Snapshot first: `DashMap::iter` holds a shard guard, and the shard
        // lock is a blocking std lock. Awaiting `is_terminal()` while holding
        // one would park a worker thread that a concurrent insert/remove needs.
        let candidates: Vec<Arc<S>> = self.sessions.iter().map(|r| r.value().clone()).collect();

        let mut to_remove = Vec::new();
        for session in candidates {
            let expired = now_ms.saturating_sub(session.last_activity_ms()) > timeout_ms;
            if expired || session.is_terminal().await {
                to_remove.push(session.id().to_string());
            }
        }

        for id in to_remove {
            tracing::info!("Cleaning up idle {} session: {}", S::kind(), id);
            self.sessions.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Minimal session with no provider concepts attached — exercises the
    /// registry itself rather than any one provider's wiring.
    struct FakeSession {
        id: String,
        last_activity_ms: AtomicU64,
        terminal: bool,
    }

    impl FakeSession {
        fn new(id: &str, last_activity_ms: u64, terminal: bool) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                last_activity_ms: AtomicU64::new(last_activity_ms),
                terminal,
            })
        }
    }

    #[async_trait]
    impl ManagedSession for FakeSession {
        fn id(&self) -> &str {
            &self.id
        }
        fn last_activity_ms(&self) -> u64 {
            self.last_activity_ms.load(Ordering::Relaxed)
        }
        fn kind() -> &'static str {
            "Fake"
        }
        async fn is_terminal(&self) -> bool {
            self.terminal
        }
    }

    #[tokio::test]
    async fn insert_enforces_the_session_limit() {
        let store = SessionStore::new(1);
        store
            .insert(FakeSession::new("a", now_epoch_ms(), false))
            .unwrap();

        let err = store
            .insert(FakeSession::new("b", now_epoch_ms(), false))
            .unwrap_err();

        assert!(err.to_string().contains("Concurrent session limit reached"));
        assert_eq!(store.count(), 1);
    }

    #[tokio::test]
    async fn reinserting_an_id_replaces_it_without_consuming_a_slot() {
        let store = SessionStore::new(2);
        store
            .insert(FakeSession::new("a", now_epoch_ms(), false))
            .unwrap();

        store
            .insert(FakeSession::new("a", now_epoch_ms(), false))
            .unwrap();

        assert_eq!(store.count(), 1);
    }

    /// Documents a quirk carried over from the stores this replaced: the
    /// capacity check looks at occupancy only, so at capacity even a
    /// same-id insert is refused. Unreachable with UUID session ids.
    #[tokio::test]
    async fn at_capacity_even_a_same_id_insert_is_refused() {
        let store = SessionStore::new(1);
        store
            .insert(FakeSession::new("a", now_epoch_ms(), false))
            .unwrap();

        assert!(store
            .insert(FakeSession::new("a", now_epoch_ms(), false))
            .is_err());
    }

    #[tokio::test]
    async fn cleanup_reaps_idle_and_terminal_sessions() {
        let store = SessionStore::new(8);
        store.insert(FakeSession::new("stale", 0, false)).unwrap();
        store
            .insert(FakeSession::new("terminal", now_epoch_ms(), true))
            .unwrap();
        store
            .insert(FakeSession::new("live", now_epoch_ms(), false))
            .unwrap();

        store.run_cleanup(60).await;

        let ids: Vec<String> = store.list().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids, vec!["live".to_string()]);
    }

    /// A zero timeout must not reap a session that was just active: the
    /// comparison is strictly greater-than.
    #[tokio::test]
    async fn zero_timeout_keeps_a_session_active_this_instant() {
        let store = SessionStore::new(4);
        store
            .insert(FakeSession::new("now", now_epoch_ms(), false))
            .unwrap();

        store.run_cleanup(0).await;

        assert_eq!(store.count(), 1);
    }

    /// The snapshot in `run_cleanup` exists so no shard guard is held across
    /// the `is_terminal()` await.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cleanup_does_not_block_concurrent_mutation() {
        let store = Arc::new(SessionStore::new(512));
        for i in 0..128 {
            store
                .insert(FakeSession::new(&format!("seed{i}"), 0, false))
                .unwrap();
        }

        let writer = {
            let store = store.clone();
            tokio::spawn(async move {
                for i in 0..512 {
                    let _ = store.insert(FakeSession::new(&format!("w{i}"), now_epoch_ms(), false));
                    store.remove(&format!("w{i}"));
                    tokio::task::yield_now().await;
                }
            })
        };
        let cleaner = {
            let store = store.clone();
            tokio::spawn(async move {
                for _ in 0..64 {
                    store.run_cleanup(60).await;
                    tokio::task::yield_now().await;
                }
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::try_join!(writer, cleaner)
        })
        .await
        .expect("cleanup and mutation starved each other")
        .expect("task panicked");

        assert_eq!(store.count(), 0);
    }
}
