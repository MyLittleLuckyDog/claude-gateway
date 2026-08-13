use async_trait::async_trait;

use super::{Session, SessionState};
use crate::core::session::ManagedSession;

/// Claude CLI-wrap sessions.
pub type SessionStore = crate::core::session::SessionStore<Session>;

#[async_trait]
impl ManagedSession for Session {
    fn id(&self) -> &str {
        &self.id
    }

    fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn kind() -> &'static str {
        "Claude"
    }

    async fn is_terminal(&self) -> bool {
        *self.state.lock().await == SessionState::Dead
    }

    async fn shutdown(&self) {
        // A permit is stored if the loop is not parked on `notified()` right
        // now, so a delete that lands mid-turn still stops it.
        self.cancel.notify_one();
    }
}
