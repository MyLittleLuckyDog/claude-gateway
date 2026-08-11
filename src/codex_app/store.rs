use async_trait::async_trait;

use super::session::{CodexAppSession, CodexAppSessionState};
use crate::core::session::ManagedSession;

/// Codex `app-server` sessions.
pub type CodexAppSessionStore = crate::core::session::SessionStore<CodexAppSession>;

#[async_trait]
impl ManagedSession for CodexAppSession {
    fn id(&self) -> &str {
        &self.id
    }

    fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn kind() -> &'static str {
        "Codex app"
    }

    async fn is_terminal(&self) -> bool {
        *self.state.lock().await == CodexAppSessionState::Dead
    }
}
