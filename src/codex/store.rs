use async_trait::async_trait;

use crate::codex::session::{CodexSession, CodexSessionState};
use crate::core::session::ManagedSession;

/// Codex `exec` sessions.
pub type CodexSessionStore = crate::core::session::SessionStore<CodexSession>;

#[async_trait]
impl ManagedSession for CodexSession {
    fn id(&self) -> &str {
        &self.id
    }

    fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn kind() -> &'static str {
        "Codex"
    }

    async fn is_terminal(&self) -> bool {
        *self.state.lock().await == CodexSessionState::Dead
    }
}
