use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::codex::options::CodexOptions;

pub const MAX_CODEX_APP_HISTORY_SIZE: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub enum CodexAppSessionState {
    Initializing,
    Idle,
    Running,
    WaitingForApproval { request_id: String, method: String },
    Dead,
}

impl std::fmt::Display for CodexAppSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initializing => write!(f, "initializing"),
            Self::Idle => write!(f, "idle"),
            Self::Running => write!(f, "running"),
            Self::WaitingForApproval { .. } => write!(f, "waiting_for_approval"),
            Self::Dead => write!(f, "dead"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: String,
    pub method: String,
}

pub struct CodexAppSession {
    pub id: String,
    pub thread_id: Arc<Mutex<Option<String>>>,
    pub turn_id: Arc<Mutex<Option<String>>>,
    pub state: Arc<Mutex<CodexAppSessionState>>,
    pub created_at: std::time::Instant,
    pub last_activity_ms: AtomicU64,
    pub options: CodexOptions,
    pub stdin_tx: mpsc::Sender<String>,
    pub event_tx: broadcast::Sender<Arc<Value>>,
    pub history: Arc<Mutex<VecDeque<Arc<Value>>>>,
    pub pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    pub pending_approval: Arc<Mutex<Option<PendingApproval>>>,
    pub next_request_id: AtomicU64,
}
