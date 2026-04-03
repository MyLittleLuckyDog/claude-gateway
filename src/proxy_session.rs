//! Proxy-mode multi-turn sessions.
//! Manages conversation history (messages array) server-side,
//! tracks token usage, and auto-cleans when context limit approaches.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::models;

/// Maximum concurrent proxy sessions
const MAX_PROXY_SESSIONS: usize = 50;

/// Session idle timeout (seconds) — matches CLI default
const SESSION_IDLE_TIMEOUT_SECS: u64 = 1800; // 30 min

// ── Session ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ProxySession {
    pub id: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    pub messages: Vec<serde_json::Value>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub created_at: u64,
    pub last_activity: u64,
    #[serde(skip)]
    pub options: SessionOptions,
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
    pub fn new(id: String, options: SessionOptions) -> Self {
        let model = options.model.as_deref()
            .map(|m| models::canonical_model_id(m).to_string())
            .unwrap_or_else(|| models::DEFAULT_MODEL.id.to_string());

        Self {
            id,
            model,
            system: options.system.clone(),
            messages: Vec::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            created_at: epoch_secs(),
            last_activity: epoch_secs(),
            options,
        }
    }

    /// Estimated total tokens in context (input + output so far)
    pub fn estimated_context_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }

    /// Is the context approaching the limit?
    pub fn is_context_near_limit(&self) -> bool {
        self.estimated_context_tokens() >= models::CONTEXT_CLEANUP_THRESHOLD as u64
    }

    /// Build the API request body for the current state + new user content.
    pub fn build_request(&self, max_tokens_override: Option<u32>) -> serde_json::Value {
        let max_tokens = max_tokens_override
            .or(self.options.max_tokens)
            .unwrap_or_else(|| models::default_max_tokens(&self.model));

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "messages": self.messages,
        });

        let obj = body.as_object_mut().expect("just created");

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

        body
    }

    /// Append a user message to the conversation.
    pub fn add_user_message(&mut self, content: serde_json::Value) {
        self.messages.push(serde_json::json!({
            "role": "user",
            "content": content,
        }));
        self.last_activity = epoch_secs();
    }

    /// Append a tool_result message to the conversation.
    pub fn add_tool_result(&mut self, tool_results: Vec<serde_json::Value>) {
        self.messages.push(serde_json::json!({
            "role": "user",
            "content": tool_results,
        }));
        self.last_activity = epoch_secs();
    }

    /// Record assistant response and update token counts.
    pub fn record_assistant_response(
        &mut self,
        response: &serde_json::Value,
    ) {
        // Extract content blocks from response
        if let Some(content) = response.get("content") {
            self.messages.push(serde_json::json!({
                "role": "assistant",
                "content": content,
            }));
        }

        // Update token counts
        if let Some(usage) = response.get("usage") {
            if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                self.total_input_tokens += input;
            }
            if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                self.total_output_tokens += output;
            }
        }

        self.last_activity = epoch_secs();
    }

    /// Check if session has been idle too long
    fn is_idle(&self) -> bool {
        epoch_secs() - self.last_activity > SESSION_IDLE_TIMEOUT_SECS
    }
}

// ── Session Store ──────────────────────────────────────────────────

pub struct ProxySessionStore {
    sessions: RwLock<HashMap<String, ProxySession>>,
}

impl ProxySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub async fn create(&self, options: SessionOptions) -> Result<ProxySession, String> {
        let mut sessions = self.sessions.write().await;

        if sessions.len() >= MAX_PROXY_SESSIONS {
            return Err(format!("Max proxy sessions ({MAX_PROXY_SESSIONS}) reached"));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let session = ProxySession::new(id.clone(), options);
        sessions.insert(id, session.clone());

        Ok(session)
    }

    pub async fn get(&self, id: &str) -> Option<ProxySession> {
        self.sessions.read().await.get(id).cloned()
    }

    pub async fn update(&self, session: ProxySession) {
        self.sessions.write().await.insert(session.id.clone(), session);
    }

    pub async fn delete(&self, id: &str) -> bool {
        self.sessions.write().await.remove(id).is_some()
    }

    pub async fn list(&self) -> Vec<SessionSummary> {
        self.sessions.read().await.values().map(|s| SessionSummary {
            id: s.id.clone(),
            model: s.model.clone(),
            messages_count: s.messages.len(),
            total_input_tokens: s.total_input_tokens,
            total_output_tokens: s.total_output_tokens,
            context_near_limit: s.is_context_near_limit(),
            created_at: s.created_at,
            last_activity: s.last_activity,
        }).collect()
    }

    /// Remove idle and context-exhausted sessions
    pub async fn cleanup(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();

        sessions.retain(|_id, s| {
            if s.is_idle() {
                tracing::info!("Cleaning up idle proxy session {}", s.id);
                return false;
            }
            true
        });

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
    pub context_near_limit: bool,
    pub created_at: u64,
    pub last_activity: u64,
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
