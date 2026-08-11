pub mod store;

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::core::events::Seq;
use crate::messages::Message;
use crate::options::ClaudeAgentOptions;

#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Initializing,
    Idle,
    Running,
    WaitingForHook {
        request_id: String,
        deadline: std::time::Instant,
    },
    WaitingForPermission {
        request_id: String,
        original_input: serde_json::Value,
    },
    Dead,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initializing => write!(f, "initializing"),
            Self::Idle => write!(f, "idle"),
            Self::Running => write!(f, "running"),
            Self::WaitingForHook { .. } => write!(f, "waiting_for_hook"),
            Self::WaitingForPermission { .. } => write!(f, "waiting_for_permission"),
            Self::Dead => write!(f, "dead"),
        }
    }
}

pub struct Session {
    pub id: String,
    pub cli_session_id: Arc<Mutex<Option<String>>>,
    pub state: Arc<Mutex<SessionState>>,
    pub created_at: std::time::Instant,
    /// Epoch millis of last activity — lock-free via AtomicU64.
    pub last_activity_ms: AtomicU64,
    pub options: ClaudeAgentOptions,
    pub stdin_tx: mpsc::Sender<String>,
    pub event_tx: broadcast::Sender<Seq<Message>>,
    pub history: Arc<Mutex<VecDeque<Seq<Message>>>>,
    /// Next sequence number for this session's events. Stable across
    /// reconnects, so clients can resume with `Last-Event-ID`.
    pub next_seq: AtomicU64,
    /// Handle for the pending hook timeout task (if any). Aborted on hook_response.
    pub hook_timeout_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}
