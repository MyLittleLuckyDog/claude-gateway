use std::sync::Arc;

use dashmap::DashMap;

use crate::codex::session::{CodexSession, CodexSessionState};
use crate::error::GatewayError;

pub struct CodexSessionStore {
    sessions: DashMap<String, Arc<CodexSession>>,
    max_sessions: usize,
    op_lock: std::sync::Mutex<()>,
}

impl CodexSessionStore {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: DashMap::new(),
            max_sessions,
            op_lock: std::sync::Mutex::new(()),
        }
    }

    pub fn insert(&self, session: Arc<CodexSession>) -> Result<(), GatewayError> {
        let _guard = self.op_lock.lock().unwrap();
        if self.sessions.len() >= self.max_sessions {
            return Err(GatewayError::SessionLimitReached { max: self.max_sessions });
        }
        self.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Arc<CodexSession>, GatewayError> {
        self.sessions
            .get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| GatewayError::SessionNotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<Arc<CodexSession>> {
        self.sessions.iter().map(|r| r.value().clone()).collect()
    }

    pub fn remove(&self, id: &str) -> bool {
        let _guard = self.op_lock.lock().unwrap();
        self.sessions.remove(id).is_some()
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
            if (now_ms.saturating_sub(last_ms)) > timeout_ms || state == CodexSessionState::Dead {
                to_remove.push(entry.key().clone());
            }
        }

        for id in to_remove {
            tracing::info!("Cleaning up idle Codex session: {}", id);
            self.sessions.remove(&id);
        }
    }
}
