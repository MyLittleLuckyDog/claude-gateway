use std::sync::Arc;
use dashmap::DashMap;
use crate::error::GatewayError;
use super::Session;

pub struct SessionStore {
    sessions: DashMap<String, Arc<Session>>,
    max_sessions: usize,
}

impl SessionStore {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: DashMap::new(),
            max_sessions,
        }
    }

    pub fn insert(&self, session: Arc<Session>) -> Result<(), GatewayError> {
        if self.sessions.len() >= self.max_sessions {
            return Err(GatewayError::SessionLimitReached { max: self.max_sessions });
        }
        self.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Arc<Session>, GatewayError> {
        self.sessions.get(id)
            .map(|r| r.value().clone())
            .ok_or_else(|| GatewayError::SessionNotFound(id.to_string()))
    }

    pub fn remove(&self, id: &str) -> bool {
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
            let last_ms = session.last_activity_ms.load(std::sync::atomic::Ordering::Relaxed);
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
