use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio::sync::{broadcast, Mutex};

use crate::codex::messages::CodexEvent;
use crate::codex::options::CodexOptions;

pub const MAX_CODEX_HISTORY_SIZE: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub enum CodexSessionState {
    Idle,
    Running,
    Dead,
}

impl std::fmt::Display for CodexSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Running => write!(f, "running"),
            Self::Dead => write!(f, "dead"),
        }
    }
}

pub struct CodexSession {
    pub id: String,
    pub thread_id: Arc<Mutex<Option<String>>>,
    pub state: Arc<Mutex<CodexSessionState>>,
    pub created_at: std::time::Instant,
    pub last_activity_ms: AtomicU64,
    pub options: CodexOptions,
    pub event_tx: broadcast::Sender<Arc<CodexEvent>>,
    pub history: Arc<Mutex<VecDeque<Arc<CodexEvent>>>>,
}
