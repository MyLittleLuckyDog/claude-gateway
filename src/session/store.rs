use super::Session;
use crate::error::GatewayError;
use dashmap::DashMap;
use std::sync::Arc;

pub struct SessionStore {
    sessions: DashMap<String, Arc<Session>>,
    max_sessions: usize,
    op_lock: std::sync::Mutex<()>,
}

impl SessionStore {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: DashMap::new(),
            max_sessions,
            op_lock: std::sync::Mutex::new(()),
        }
    }

    pub fn insert(&self, session: Arc<Session>) -> Result<(), GatewayError> {
        let _guard = self.op_lock.lock().unwrap();
        if self.sessions.len() >= self.max_sessions {
            return Err(GatewayError::SessionLimitReached {
                max: self.max_sessions,
            });
        }
        self.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Arc<Session>, GatewayError> {
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

    pub fn list(&self) -> Vec<Arc<Session>> {
        self.sessions.iter().map(|r| r.value().clone()).collect()
    }

    pub async fn run_cleanup(&self, idle_timeout_secs: u64) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let timeout_ms = idle_timeout_secs * 1000;
        let mut to_remove = Vec::new();

        for entry in self.sessions.iter() {
            let session = entry.value();
            let last_ms = session
                .last_activity_ms
                .load(std::sync::atomic::Ordering::Relaxed);
            let state = session.state.lock().await.clone();
            if (now_ms.saturating_sub(last_ms)) > timeout_ms || state == super::SessionState::Dead {
                to_remove.push(entry.key().clone());
            }
        }

        for id in to_remove {
            tracing::info!("Cleaning up idle session: {}", id);
            self.sessions.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    use tokio::sync::{broadcast, mpsc, Mutex};

    use super::SessionStore;
    use crate::options::ClaudeAgentOptions;
    use crate::session::{Session, SessionState};

    fn make_session(id: &str) -> Arc<Session> {
        let (stdin_tx, _) = mpsc::channel::<String>(1);
        let (event_tx, _) = broadcast::channel(1);
        Arc::new(Session {
            id: id.to_string(),
            cli_session_id: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(SessionState::Idle)),
            created_at: std::time::Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            options: ClaudeAgentOptions::default(),
            stdin_tx,
            event_tx,
            history: Arc::new(Mutex::new(VecDeque::new())),
            hook_timeout_handle: Arc::new(Mutex::new(None)),
        })
    }

    #[test]
    fn insert_enforces_max_sessions_inside_store() {
        let store = SessionStore::new(1);
        store.insert(make_session("a")).unwrap();
        let err = store.insert(make_session("b")).unwrap_err();
        assert!(err.to_string().contains("Concurrent session limit reached"));
    }
}
