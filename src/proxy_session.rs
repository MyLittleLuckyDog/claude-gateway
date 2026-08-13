//! Proxy-mode multi-turn sessions.
//! Manages conversation history (messages array) server-side,
//! tracks token usage, and auto-cleans when context limit approaches.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};

use crate::models;

// ── Session ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProxySession {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    pub messages: Vec<serde_json::Value>,
    /// Cumulative input tokens across all turns (for stats)
    pub total_input_tokens: u64,
    /// Cumulative output tokens across all turns (for stats)
    pub total_output_tokens: u64,
    /// Input tokens from the most recent API response — approximates
    /// current context size (the server re-counts all messages each turn).
    pub last_input_tokens: u64,
    /// Output tokens from the most recent API response
    pub last_output_tokens: u64,
    pub created_at: u64,
    pub last_activity: u64,
    #[serde(skip)]
    pub options: SessionOptions,
    /// Idle timeout in seconds — from config
    #[serde(skip)]
    idle_timeout_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionOptions {
    /// Model ID or alias (resolved to canonical at creation)
    #[serde(default)]
    pub model: Option<String>,
    /// System prompt — string or structured blocks
    #[serde(default)]
    pub system: Option<serde_json::Value>,
    /// Default max_tokens per turn
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Temperature
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Additional beta headers
    #[serde(default)]
    pub betas: Option<Vec<String>>,
    /// Tools definitions for tool_use
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    /// Tool choice
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
}

impl ProxySession {
    pub fn new(id: String, options: SessionOptions, idle_timeout_secs: u64) -> Self {
        let model = options
            .model
            .as_deref()
            .map(|m| models::canonical_model_id(m).to_string())
            .unwrap_or_else(|| models::DEFAULT_MODEL.id.to_string());

        Self {
            id,
            model,
            system: options.system.clone(),
            messages: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            last_input_tokens: 0,
            last_output_tokens: 0,
            created_at: epoch_secs(),
            last_activity: epoch_secs(),
            options,
            idle_timeout_secs,
        }
    }

    /// Estimated current context size based on the most recent API response.
    /// After the first turn, `last_input_tokens` reflects the full context
    /// the server received (all messages re-counted), plus the output the
    /// server just generated (which is now part of the context for the next
    /// turn).
    pub fn estimated_context_tokens(&self) -> u64 {
        self.last_input_tokens + self.last_output_tokens
    }

    /// Is the context approaching the limit?
    pub fn is_context_near_limit(&self) -> bool {
        self.estimated_context_tokens() >= models::context_cleanup_threshold(&self.model) as u64
    }

    /// Preflight check: will the next request likely exceed the context window?
    pub fn preflight_check(&self, max_tokens: u32) -> Result<(), String> {
        let estimated = self.estimated_context_tokens();
        if estimated == 0 {
            // First turn — no prior data, allow it
            return Ok(());
        }
        let model_context = models::context_window(&self.model) as u64;
        if estimated + max_tokens as u64 > model_context {
            return Err(format!(
                "Estimated context ({} tokens) + max_tokens ({}) exceeds \
                 model context window ({}). Create a new session.",
                estimated, max_tokens, model_context
            ));
        }
        Ok(())
    }

    /// Build the API request body for the current state.
    /// Strips internal `_msg_id` fields from messages before sending
    /// to the Anthropic API (which rejects unknown fields).
    pub fn build_request(&self, max_tokens_override: Option<u32>) -> serde_json::Value {
        let max_tokens = max_tokens_override
            .or(self.options.max_tokens)
            .unwrap_or_else(|| models::default_max_tokens(&self.model));

        let clean_messages: Vec<serde_json::Value> = self
            .messages
            .iter()
            .map(|m| {
                let mut msg = m.clone();
                if let Some(obj) = msg.as_object_mut() {
                    obj.remove("_msg_id");
                }
                msg
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": clean_messages,
        });

        if let Some(obj) = body.as_object_mut() {
            if let Some(ref system) = self.system {
                obj.insert("system".to_string(), system.clone());
            }
            if let Some(ref temp) = self.options.temperature {
                obj.insert("temperature".to_string(), serde_json::json!(temp));
            }
            if let Some(ref tools) = self.options.tools {
                obj.insert("tools".to_string(), serde_json::json!(tools));
            }
            if let Some(ref tc) = self.options.tool_choice {
                obj.insert("tool_choice".to_string(), tc.clone());
            }
        }

        body
    }

    /// Append a user message to the conversation.
    /// Returns a unique message ID for safe rollback.
    pub fn add_user_message(&mut self, content: serde_json::Value) -> String {
        let msg_id = uuid::Uuid::new_v4().to_string();
        self.messages.push(serde_json::json!({
            "_msg_id": msg_id,
            "role": "user",
            "content": content,
        }));
        self.last_activity = epoch_secs();
        msg_id
    }

    /// Append a tool_result message to the conversation.
    /// Returns a unique message ID for safe rollback.
    pub fn add_tool_result(&mut self, tool_results: Vec<serde_json::Value>) -> String {
        let msg_id = uuid::Uuid::new_v4().to_string();
        self.messages.push(serde_json::json!({
            "_msg_id": msg_id,
            "role": "user",
            "content": tool_results,
        }));
        self.last_activity = epoch_secs();
        msg_id
    }

    /// Record assistant response and update token counts.
    pub fn record_assistant_response(&mut self, response: &serde_json::Value) {
        if let Some(content) = response.get("content") {
            self.messages.push(serde_json::json!({
                "role": "assistant",
                "content": content,
            }));
        }

        if let Some(usage) = response.get("usage") {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            self.total_input_tokens += input;
            self.total_output_tokens += output;
            // Overwrite — last turn's counts approximate current context
            self.last_input_tokens = input;
            self.last_output_tokens = output;
        }

        self.last_activity = epoch_secs();
    }

    /// Remove a message by its unique `_msg_id`. Safe under concurrent
    /// access: even if other messages were inserted/removed in the
    /// meantime, we always target the exact message we added.
    pub fn rollback_message_by_id(&mut self, msg_id: &str) -> bool {
        if let Some(pos) = self
            .messages
            .iter()
            .position(|m| m.get("_msg_id").and_then(|v| v.as_str()) == Some(msg_id))
        {
            self.messages.remove(pos);
            true
        } else {
            false
        }
    }

    /// Check if session has been idle too long
    fn is_idle(&self) -> bool {
        epoch_secs() - self.last_activity > self.idle_timeout_secs
    }
}

// ── Session Store ──────────────────────────────────────────────────

/// Thread-safe session store. Each session is wrapped in `Arc<Mutex>`
/// to prevent lost-update races when concurrent requests mutate the
/// same session.
pub struct ProxySessionStore {
    sessions: RwLock<HashMap<String, Arc<Mutex<ProxySession>>>>,
    max_sessions: usize,
    idle_timeout_secs: u64,
}

impl ProxySessionStore {
    pub fn new(max_sessions: usize, idle_timeout_secs: u64) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            max_sessions,
            idle_timeout_secs,
        }
    }

    /// Create a new session. Returns a snapshot (clone) for the response.
    pub async fn create(&self, options: SessionOptions) -> Result<ProxySession, String> {
        let mut sessions = self.sessions.write().await;

        if sessions.len() >= self.max_sessions {
            return Err(format!(
                "Max proxy sessions ({}) reached",
                self.max_sessions
            ));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let session = ProxySession::new(id.clone(), options, self.idle_timeout_secs);
        let snapshot = session.clone();
        sessions.insert(id, Arc::new(Mutex::new(session)));

        Ok(snapshot)
    }

    /// Execute a closure while holding the per-session Mutex.
    /// The outer RwLock is only held for the HashMap lookup, not during
    /// the closure execution — other sessions remain accessible.
    pub async fn with_session<F, R>(&self, id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut ProxySession) -> R,
    {
        let session_arc = {
            let sessions = self.sessions.read().await;
            sessions.get(id)?.clone()
        };
        let mut session = session_arc.lock().await;
        Some(f(&mut session))
    }

    /// Get a read-only snapshot (clone) of a session.
    pub async fn get_snapshot(&self, id: &str) -> Option<ProxySession> {
        let session_arc = {
            let sessions = self.sessions.read().await;
            sessions.get(id)?.clone()
        };
        let session = session_arc.lock().await;
        Some(session.clone())
    }

    pub async fn delete(&self, id: &str) -> bool {
        self.sessions.write().await.remove(id).is_some()
    }

    pub async fn list(&self) -> Vec<SessionSummary> {
        // Collect Arc clones under the RwLock, then release it before
        // locking individual sessions — avoids holding the RwLock across
        // await points which would block create/delete.
        let arcs: Vec<Arc<Mutex<ProxySession>>> = {
            let sessions = self.sessions.read().await;
            sessions.values().cloned().collect()
        };

        let mut summaries = Vec::with_capacity(arcs.len());
        for arc in &arcs {
            let s = arc.lock().await;
            summaries.push(SessionSummary {
                id: s.id.clone(),
                model: s.model.clone(),
                messages_count: s.messages.len(),
                total_input_tokens: s.total_input_tokens,
                total_output_tokens: s.total_output_tokens,
                estimated_context_tokens: s.estimated_context_tokens(),
                context_near_limit: s.is_context_near_limit(),
                created_at: s.created_at,
                last_activity: s.last_activity,
            });
        }
        summaries
    }

    /// Remove idle sessions. Returns count of removed sessions.
    pub async fn cleanup(&self) -> usize {
        // Phase 1: identify idle sessions without holding the write lock
        let candidates: Vec<(String, Arc<Mutex<ProxySession>>)> = {
            let sessions = self.sessions.read().await;
            sessions
                .iter()
                .map(|(id, arc)| (id.clone(), arc.clone()))
                .collect()
        };

        let mut to_remove = Vec::new();
        for (id, arc) in &candidates {
            let s = arc.lock().await;
            if s.is_idle() {
                tracing::info!("Cleaning up idle proxy session {}", s.id);
                to_remove.push(id.clone());
            }
        }

        if to_remove.is_empty() {
            return 0;
        }

        // Phase 2: remove under write lock
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        for id in &to_remove {
            sessions.remove(id);
        }
        before - sessions.len()
    }

    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub model: String,
    pub messages_count: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub estimated_context_tokens: u64,
    pub context_near_limit: bool,
    pub created_at: u64,
    pub last_activity: u64,
}

pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
