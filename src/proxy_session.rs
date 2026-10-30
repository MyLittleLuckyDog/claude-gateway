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
    /// Put one prompt-cache breakpoint on the last content block of the
    /// conversation on every request, as Claude Code does, so each turn reads
    /// the history written by the previous one instead of re-billing it.
    /// Off by default: it changes how a session is billed (cache writes cost
    /// more than plain input; reads cost far less).
    #[serde(default)]
    pub cache_breakpoint: bool,
    /// Clear old tool results once the session's context reaches
    /// `trigger_tokens`. Off unless set — there is no default trigger.
    #[serde(default)]
    pub microcompact: Option<MicrocompactOptions>,
}

/// Claude Code's "keep recent" microcompact (2.1.241 bundle: `afl`/`hKf`/`deo`),
/// run by the gateway on its own trigger. In the CLI the trigger is a
/// server-side `context_hint` exchange; the server rejects that beta for
/// subscription traffic (HTTP 400), so there is no server signal to follow and
/// the caller states the threshold instead.
#[derive(Debug, Clone, Deserialize)]
pub struct MicrocompactOptions {
    /// Run when the estimated context (last turn's input incl. cached tokens +
    /// output) is at least this many tokens.
    pub trigger_tokens: u64,
    /// Tool names whose results may be cleared. Defaults to the tools Claude
    /// Code itself compacts.
    #[serde(default)]
    pub compactable_tools: Option<Vec<String>>,
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

    /// Preflight check: is the session already out of room?
    ///
    /// `estimated_context_tokens` is what the server counted for the previous
    /// turn, answer included, so the next request holds at least that much. If
    /// that already fills the window, the request cannot fit and is refused
    /// here, without a round trip.
    ///
    /// `max_tokens` is deliberately not part of the sum. Upstream does not
    /// reject a request whose `input + max_tokens` exceeds the window: it
    /// generates until the window is full and reports
    /// `stop_reason: model_context_window_exceeded` (checked live on
    /// 2026-10-30 -- 148,816 input tokens + `max_tokens` 64,000 on the 200k
    /// models Haiku 4.5, Sonnet 4.5 and Opus 4.5 were all accepted). Counting it
    /// here refused requests the server would have served, on an estimate that
    /// cannot know the size of the message being added. Where a model does
    /// reject the sum, upstream says so with exact numbers and `messages_sync`
    /// retries with a `max_tokens` that fits.
    pub fn preflight_check(&self) -> Result<(), String> {
        let estimated = self.estimated_context_tokens();
        let model_context = models::context_window(&self.model) as u64;
        if estimated > 0 && estimated >= model_context {
            return Err(format!(
                "The session's context ({estimated} tokens at the last response) already \
                 fills the model's context window ({model_context}). Create a new session."
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

        // Applied to the outgoing copy only — stored history never carries
        // the marker, so it always sits on the current last block.
        if self.options.cache_breakpoint {
            mark_cache_breakpoint(&mut body);
        }

        body
    }

    /// Find the old tool results a microcompact pass would clear, if the session
    /// asked for one and its context has reached the caller's `trigger_tokens`.
    ///
    /// The trigger is judged on `estimated_context_tokens`, which is what the
    /// server counted for the previous turn. Nothing here measures a message:
    /// how many tokens clearing frees is counted upstream by the caller (see
    /// [`MicrocompactPlan`]) and handed to [`Self::apply_microcompact`].
    pub fn plan_microcompact(&self) -> Option<MicrocompactPlan> {
        let opts = self.options.microcompact.as_ref()?;
        if self.estimated_context_tokens() < opts.trigger_tokens {
            return None;
        }
        let allowed = &opts.compactable_tools;
        let compactable = |name: &str| match allowed {
            Some(list) => list.iter().any(|t| t == name),
            None => DEFAULT_COMPACTABLE_TOOLS.contains(&name),
        };
        let (tool_use_ids, blocks) = plan_microcompact_messages(&self.messages, &compactable)?;
        Some(MicrocompactPlan {
            model: self.model.clone(),
            tool_use_ids,
            blocks,
        })
    }

    /// Clear the planned results, which upstream counted as `tokens_freed`.
    ///
    /// Rewrites stored history for good, as the CLI does. Clearing tool results
    /// invalidates the prompt cache from the first cleared block onward, so the
    /// next request re-writes that part. The context estimate drops by the
    /// freed tokens so the preflight check sees the smaller context; the next
    /// response replaces it with the real count. Results are found by tool-use
    /// id, so messages added since the plan was made do no harm.
    pub fn apply_microcompact(
        &mut self,
        plan: &MicrocompactPlan,
        tokens_freed: u64,
    ) -> Option<MicrocompactOutcome> {
        let cleared = clear_tool_results(&mut self.messages, &plan.tool_use_ids);
        if cleared == 0 {
            return None;
        }
        self.last_input_tokens = self.last_input_tokens.saturating_sub(tokens_freed);
        tracing::info!(
            "[microcompact] session={} cleared={} tokens_freed={}",
            self.id,
            cleared,
            tokens_freed
        );
        Some(MicrocompactOutcome {
            cleared_tool_results: cleared,
            tokens_freed,
        })
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
            // `input_tokens` excludes prompt-cache tokens: with caching on, it
            // is only the uncached tail. The context the model actually saw
            // is the sum, so count all three or the size estimate (and with it
            // the preflight check) collapses as soon as the cache works.
            let cached = ["cache_creation_input_tokens", "cache_read_input_tokens"]
                .iter()
                .map(|k| usage.get(*k).and_then(|v| v.as_u64()).unwrap_or(0))
                .sum::<u64>();
            // Overwrite — last turn's counts approximate current context
            self.last_input_tokens = input + cached;
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

// ── Microcompact ───────────────────────────────────────────────────

/// What replaces a cleared tool result (CLI constant `U1i`).
pub const MICROCOMPACT_PLACEHOLDER: &str = "[Old tool result content cleared]";
/// Most recent compactable tool results always kept (CLI `Nfh`).
const MICROCOMPACT_KEEP_RECENT: usize = 5;
/// Nothing is cleared unless at least this many tokens would be freed (CLI
/// `sfl`). The tokens are counted by upstream (`count_tokens`), never estimated
/// from bytes or characters.
pub const MICROCOMPACT_MIN_TOKENS_FREED: u64 = 20_000;
/// Marker the CLI leaves when it persisted a cleared result to disk (`mrH`);
/// treated as already cleared.
const PERSISTED_OUTPUT_PREFIX: &str = "<persisted-output>";
/// Tools the CLI compacts: Read, Grep, Glob, WebSearch, WebFetch, Edit, Write,
/// Bash, PowerShell.
const DEFAULT_COMPACTABLE_TOOLS: [&str; 9] = [
    "Read",
    "Grep",
    "Glob",
    "WebSearch",
    "WebFetch",
    "Edit",
    "Write",
    "Bash",
    "PowerShell",
];

/// What a microcompact pass did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MicrocompactOutcome {
    pub cleared_tool_results: usize,
    /// Tokens the clearing freed, as counted by upstream.
    pub tokens_freed: u64,
}

/// The old tool results a microcompact pass would clear, found without looking
/// at their size. Whether clearing them is worth it is decided by counting
/// `count_request_before()` and `count_request_after()` upstream.
#[derive(Debug, Clone)]
pub struct MicrocompactPlan {
    model: String,
    tool_use_ids: Vec<String>,
    /// The content of those results, as plain user content blocks.
    blocks: Vec<serde_json::Value>,
}

impl MicrocompactPlan {
    /// A `count_tokens` body holding the content as it is now.
    pub fn count_request_before(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": self.blocks}],
        })
    }

    /// A `count_tokens` body holding what replaces it: one placeholder per
    /// cleared result. Subtracting its count from the "before" count cancels
    /// the fixed per-message overhead, leaving exactly the tokens freed.
    pub fn count_request_after(&self) -> serde_json::Value {
        let placeholders: Vec<serde_json::Value> = self
            .tool_use_ids
            .iter()
            .map(|_| serde_json::json!({"type": "text", "text": MICROCOMPACT_PLACEHOLDER}))
            .collect();
        serde_json::json!({
            "model": self.model,
            "messages": [{"role": "user", "content": placeholders}],
        })
    }
}

/// CLI `brH`: already cleared (or persisted elsewhere).
fn is_already_cleared(block: &serde_json::Value) -> bool {
    block
        .get("content")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|c| c == MICROCOMPACT_PLACEHOLDER || c.starts_with(PERSISTED_OUTPUT_PREFIX))
}

/// A tool result's content as plain user content blocks (a string becomes one
/// text block). `None` when there is nothing in it to clear.
fn result_content_blocks(block: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match block.get("content") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            Some(vec![serde_json::json!({"type": "text", "text": s})])
        }
        Some(serde_json::Value::Array(parts)) if !parts.is_empty() => Some(parts.clone()),
        _ => None,
    }
}

/// Pick the old compactable tool results in `messages`: every compactable tool
/// call but the `MICROCOMPACT_KEEP_RECENT` most recent, whose result is not
/// already cleared and has something in it. Returns their ids and content.
fn plan_microcompact_messages(
    messages: &[serde_json::Value],
    compactable: &dyn Fn(&str) -> bool,
) -> Option<(Vec<String>, Vec<serde_json::Value>)> {
    // Ids of compactable tool calls, oldest first.
    let ids: Vec<String> = messages
        .iter()
        .filter(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))
        .filter_map(|m| m.get("content").and_then(serde_json::Value::as_array))
        .flatten()
        .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"))
        .filter(|b| {
            b.get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(compactable)
        })
        .filter_map(|b| b.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();

    let keep_from = ids.len().saturating_sub(MICROCOMPACT_KEEP_RECENT);
    let stale: std::collections::HashSet<&str> =
        ids[..keep_from].iter().map(String::as_str).collect();
    if stale.is_empty() {
        return None;
    }

    let mut tool_use_ids = Vec::new();
    let mut blocks = Vec::new();
    for m in messages {
        if m.get("role").and_then(serde_json::Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = m.get("content").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for b in content {
            let id = b
                .get("tool_use_id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| stale.contains(id));
            let is_tool_result =
                b.get("type").and_then(serde_json::Value::as_str) == Some("tool_result");
            if let (true, Some(id), false) = (is_tool_result, id, is_already_cleared(b)) {
                if let Some(content_blocks) = result_content_blocks(b) {
                    tool_use_ids.push(id.to_string());
                    blocks.extend(content_blocks);
                }
            }
        }
    }
    (!tool_use_ids.is_empty()).then_some((tool_use_ids, blocks))
}

/// Replace the content of the tool results with these ids by the placeholder.
/// Returns how many were cleared; results already cleared are left alone.
fn clear_tool_results(messages: &mut [serde_json::Value], ids: &[String]) -> usize {
    let mut cleared = 0;
    for m in messages.iter_mut() {
        if m.get("role").and_then(serde_json::Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = m
            .get_mut("content")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for b in content.iter_mut() {
            let wanted = b.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
                && b.get("tool_use_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| ids.iter().any(|w| w == id))
                && !is_already_cleared(b);
            if wanted {
                if let Some(obj) = b.as_object_mut() {
                    obj.insert(
                        "content".to_string(),
                        serde_json::Value::String(MICROCOMPACT_PLACEHOLDER.to_string()),
                    );
                    cleared += 1;
                }
            }
        }
    }
    cleared
}

// ── Prompt-cache breakpoint ────────────────────────────────────────

/// The API rejects a request with more than this many `cache_control` markers.
const MAX_CACHE_BREAKPOINTS: usize = 4;

fn marked_blocks(content: Option<&serde_json::Value>) -> usize {
    content
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("cache_control").is_some_and(|c| !c.is_null()))
                .count()
        })
        .unwrap_or(0)
}

/// Markers already present in `system`, `tools` and message content.
fn count_cache_breakpoints(body: &serde_json::Value) -> usize {
    let in_messages = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|msgs| msgs.iter().map(|m| marked_blocks(m.get("content"))).sum())
        .unwrap_or(0);
    marked_blocks(body.get("system")) + marked_blocks(body.get("tools")) + in_messages
}

/// Can the last content block of `msg` carry a cache marker? Thinking blocks
/// can't, and neither can an empty text block.
fn ends_in_cacheable_block(msg: &serde_json::Value) -> bool {
    match msg.get("content") {
        Some(serde_json::Value::String(s)) => !s.trim().is_empty(),
        Some(serde_json::Value::Array(blocks)) => match blocks.last() {
            Some(last) => match last.get("type").and_then(serde_json::Value::as_str) {
                Some("thinking") | Some("redacted_thinking") => false,
                Some("text") => last
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|t| !t.trim().is_empty()),
                _ => last.is_object(),
            },
            None => false,
        },
        _ => false,
    }
}

/// Mirror of Claude Code's message-cache policy (2.1.241 bundle, `gfh`/`sTE`):
/// one `ephemeral` marker, on the last content block of the last message that
/// can carry one, default 5-minute TTL. Skipped when the request already uses
/// all the breakpoints the API allows, so a caller's own markers never turn
/// into a 400.
fn mark_cache_breakpoint(body: &mut serde_json::Value) {
    if count_cache_breakpoints(body) >= MAX_CACHE_BREAKPOINTS {
        return;
    }
    let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let Some(msg) = messages
        .iter_mut()
        .rev()
        .find(|m| ends_in_cacheable_block(m))
    else {
        return;
    };
    let marker = serde_json::json!({"type": "ephemeral"});
    let Some(obj) = msg.as_object_mut() else {
        return;
    };
    match obj.get_mut("content") {
        Some(serde_json::Value::String(text)) => {
            let text = std::mem::take(text);
            obj.insert(
                "content".to_string(),
                serde_json::json!([{"type": "text", "text": text, "cache_control": marker}]),
            );
        }
        Some(serde_json::Value::Array(blocks)) => {
            if let Some(last) = blocks.last_mut().and_then(serde_json::Value::as_object_mut) {
                last.entry("cache_control").or_insert(marker);
            }
        }
        _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> ProxySession {
        ProxySession::new("s".to_string(), SessionOptions::default(), 3600)
    }

    #[test]
    fn context_estimate_counts_prompt_cache_tokens() {
        let mut s = session();
        s.record_assistant_response(&json!({
            "content": [{"type": "text", "text": "ok"}],
            "usage": {
                "input_tokens": 20,
                "cache_creation_input_tokens": 1_000,
                "cache_read_input_tokens": 150_000,
                "output_tokens": 50,
            },
        }));
        assert_eq!(s.last_input_tokens, 151_020);
        assert_eq!(s.estimated_context_tokens(), 151_070);
        // The cumulative counter stays the plain input count.
        assert_eq!(s.total_input_tokens, 20);
    }

    #[test]
    fn preflight_blocks_when_cached_context_would_overflow() {
        let mut s = session();
        let window = models::context_window(&s.model) as u64;
        s.record_assistant_response(&json!({
            "content": [],
            "usage": {
                "input_tokens": 5,
                "cache_read_input_tokens": window,
                "output_tokens": 0,
            },
        }));
        assert!(s.preflight_check().is_err());
    }

    fn options_with_cache() -> SessionOptions {
        SessionOptions {
            cache_breakpoint: true,
            ..Default::default()
        }
    }

    fn marked(body: &serde_json::Value) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (i, m) in body["messages"].as_array().unwrap().iter().enumerate() {
            if let Some(blocks) = m["content"].as_array() {
                for (j, b) in blocks.iter().enumerate() {
                    if b.get("cache_control").is_some() {
                        out.push((i, j));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn no_cache_marker_unless_opted_in() {
        let mut s = session();
        s.add_user_message(json!("hi"));
        let body = s.build_request(None);
        assert!(!body.to_string().contains("cache_control"));
    }

    #[test]
    fn marker_goes_on_last_block_of_last_message_only() {
        let mut s = ProxySession::new("s".into(), options_with_cache(), 3600);
        s.add_user_message(json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"},
        ]));
        s.record_assistant_response(&json!({
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }));
        s.add_user_message(json!("next question"));

        let body = s.build_request(None);
        // String content is lifted into a text block to carry the marker.
        assert_eq!(marked(&body), vec![(2, 0)]);
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // Stored history is untouched: the marker never accumulates.
        assert!(!s
            .messages
            .iter()
            .any(|m| m.to_string().contains("cache_control")));
    }

    #[test]
    fn marker_moves_forward_each_turn_without_stacking() {
        let mut s = ProxySession::new("s".into(), options_with_cache(), 3600);
        s.add_user_message(json!("q1"));
        assert_eq!(marked(&s.build_request(None)), vec![(0, 0)]);
        s.record_assistant_response(&json!({
            "content": [{"type": "text", "text": "a1"}],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }));
        s.add_user_message(json!("q2"));
        assert_eq!(marked(&s.build_request(None)), vec![(2, 0)]);
    }

    #[test]
    fn trailing_thinking_block_is_never_marked() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "q"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "a"},
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
            ]},
        ]});
        mark_cache_breakpoint(&mut body);
        assert_eq!(marked(&body), vec![(0, 0)]);
    }

    #[test]
    fn empty_text_block_is_never_marked() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "q"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "  "}]},
        ]});
        mark_cache_breakpoint(&mut body);
        assert_eq!(marked(&body), vec![(0, 0)]);
    }

    #[test]
    fn caller_markers_count_toward_the_api_limit() {
        let cc = json!({"type": "ephemeral"});
        let mut body = json!({
            "system": [
                {"type": "text", "text": "a", "cache_control": cc},
                {"type": "text", "text": "b", "cache_control": cc},
            ],
            "tools": [
                {"name": "t1", "input_schema": {}, "cache_control": cc},
                {"name": "t2", "input_schema": {}, "cache_control": cc},
            ],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "q"}]}],
        });
        mark_cache_breakpoint(&mut body);
        assert!(marked(&body).is_empty(), "no fifth marker may be added");

        body["tools"][1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        mark_cache_breakpoint(&mut body);
        assert_eq!(marked(&body), vec![(0, 0)]);
    }

    #[test]
    fn context_estimate_without_cache_fields_is_unchanged() {
        let mut s = session();
        s.record_assistant_response(&json!({
            "content": [],
            "usage": {"input_tokens": 100, "output_tokens": 10},
        }));
        assert_eq!(s.estimated_context_tokens(), 110);
    }

    // ── microcompact ───────────────────────────────────────────────

    /// One tool call and its result (the result text is only a label: nothing
    /// here depends on how big it is).
    fn exchange(s: &mut ProxySession, id: &str, tool: &str) {
        s.messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": "calling"},
            {"type": "tool_use", "id": id, "name": tool, "input": {"path": "/x"}},
        ]}));
        s.add_tool_result(vec![json!({
            "type": "tool_result", "tool_use_id": id, "content": format!("output of {id}"),
        })]);
    }

    fn result_content(s: &ProxySession, id: &str) -> serde_json::Value {
        for m in &s.messages {
            for b in m["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_result" && b["tool_use_id"] == id {
                    return b["content"].clone();
                }
            }
        }
        panic!("no tool_result for {id}");
    }

    /// A session that asked for microcompact at 100,000 tokens, holds `n` `Read`
    /// calls, and whose last response put the context at `context` tokens.
    fn compacting_session(n: usize, context: u64) -> ProxySession {
        let mut s = ProxySession::new(
            "s".into(),
            SessionOptions {
                microcompact: Some(MicrocompactOptions {
                    trigger_tokens: 100_000,
                    compactable_tools: None,
                }),
                ..Default::default()
            },
            3600,
        );
        for i in 0..n {
            exchange(&mut s, &format!("t{i}"), "Read");
        }
        s.last_input_tokens = context;
        s
    }

    fn planned_ids(plan: &MicrocompactPlan) -> Vec<&str> {
        plan.tool_use_ids.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_plan_keeps_the_five_most_recent_calls() {
        let s = compacting_session(8, 150_000);
        let plan = s.plan_microcompact().expect("plan");
        assert_eq!(planned_ids(&plan), ["t0", "t1", "t2"]);
        // The content to count is exactly what those results hold.
        assert_eq!(
            plan.blocks,
            [
                json!({"type": "text", "text": "output of t0"}),
                json!({"type": "text", "text": "output of t1"}),
                json!({"type": "text", "text": "output of t2"}),
            ]
        );
    }

    #[test]
    fn five_calls_or_fewer_give_no_plan() {
        assert!(compacting_session(5, 150_000).plan_microcompact().is_none());
        assert!(compacting_session(6, 150_000).plan_microcompact().is_some());
    }

    #[test]
    fn the_trigger_is_judged_on_the_servers_count_alone() {
        let mut s = compacting_session(8, 60_000);
        s.last_output_tokens = 1_000;
        assert!(s.plan_microcompact().is_none(), "61k < 100k");
        // Reaching the trigger counts the previous answer too: 99,000 + 1,000.
        s.last_input_tokens = 99_000;
        assert!(s.plan_microcompact().is_some(), "100k reaches it");
    }

    #[test]
    fn microcompact_is_off_unless_the_session_asks_for_it() {
        let mut s = session();
        for i in 0..8 {
            exchange(&mut s, &format!("t{i}"), "Read");
        }
        s.last_input_tokens = 900_000;
        assert!(s.plan_microcompact().is_none());
    }

    #[test]
    fn other_tools_neither_count_toward_keep_nor_get_planned() {
        let mut s = compacting_session(0, 150_000);
        for i in 0..8 {
            exchange(&mut s, &format!("r{i}"), "Read");
            exchange(&mut s, &format!("c{i}"), "custom_tool");
        }
        let plan = s.plan_microcompact().expect("plan");
        assert_eq!(planned_ids(&plan), ["r0", "r1", "r2"]);
    }

    #[test]
    fn the_caller_can_name_its_own_compactable_tools() {
        let mut s = compacting_session(8, 150_000);
        s.options.microcompact = Some(MicrocompactOptions {
            trigger_tokens: 1,
            compactable_tools: Some(vec!["lookup".to_string()]),
        });
        // Every call is a "Read", which the caller's list excludes.
        assert!(s.plan_microcompact().is_none());
    }

    #[test]
    fn media_in_a_result_goes_to_the_counter_as_it_is() {
        let image = json!({"type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "AA"}});
        let mut s = compacting_session(0, 150_000);
        s.messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "img", "name": "Read", "input": {}},
        ]}));
        s.add_tool_result(vec![json!({
            "type": "tool_result", "tool_use_id": "img",
            "content": [{"type": "text", "text": "a picture"}, image.clone()],
        })]);
        for i in 0..5 {
            exchange(&mut s, &format!("t{i}"), "Read");
        }
        let plan = s.plan_microcompact().expect("plan");
        assert_eq!(planned_ids(&plan), ["img"]);
        assert_eq!(
            plan.blocks,
            [json!({"type": "text", "text": "a picture"}), image]
        );
    }

    #[test]
    fn empty_and_already_cleared_results_are_not_planned() {
        let mut s = compacting_session(0, 150_000);
        exchange(&mut s, "empty", "Read");
        exchange(&mut s, "cleared", "Read");
        for i in 0..5 {
            exchange(&mut s, &format!("t{i}"), "Read");
        }
        for m in s.messages.iter_mut() {
            for b in m["content"].as_array_mut().into_iter().flatten() {
                match b["tool_use_id"].as_str() {
                    Some("empty") => b["content"] = json!(""),
                    Some("cleared") => b["content"] = json!(MICROCOMPACT_PLACEHOLDER),
                    _ => {}
                }
            }
        }
        assert!(s.plan_microcompact().is_none());
    }

    #[test]
    fn the_two_count_requests_differ_only_in_what_they_hold() {
        let s = compacting_session(8, 150_000);
        let plan = s.plan_microcompact().expect("plan");
        let before = plan.count_request_before();
        let after = plan.count_request_after();
        assert_eq!(before["model"], after["model"]);
        assert_eq!(before["messages"][0]["role"], "user");
        assert_eq!(
            before["messages"][0]["content"].as_array().map(Vec::len),
            Some(3)
        );
        // One placeholder per cleared result.
        assert_eq!(
            after["messages"][0]["content"],
            json!([
                {"type": "text", "text": MICROCOMPACT_PLACEHOLDER},
                {"type": "text", "text": MICROCOMPACT_PLACEHOLDER},
                {"type": "text", "text": MICROCOMPACT_PLACEHOLDER},
            ])
        );
    }

    #[test]
    fn applying_a_plan_clears_exactly_those_results_and_lowers_the_estimate() {
        let mut s = compacting_session(8, 150_000);
        let plan = s.plan_microcompact().expect("plan");
        let out = s.apply_microcompact(&plan, 30_000).expect("applies");
        assert_eq!(
            out,
            MicrocompactOutcome {
                cleared_tool_results: 3,
                tokens_freed: 30_000
            }
        );
        for i in 0..3 {
            assert_eq!(
                result_content(&s, &format!("t{i}")),
                MICROCOMPACT_PLACEHOLDER
            );
        }
        for i in 3..8 {
            assert_eq!(
                result_content(&s, &format!("t{i}")),
                json!(format!("output of t{i}"))
            );
        }
        assert_eq!(s.last_input_tokens, 120_000);
        // The tool calls themselves are untouched.
        assert!(s.messages.iter().any(|m| m["content"]
            .as_array()
            .is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use" && b["id"] == "t0"))));
    }

    #[test]
    fn applying_a_plan_twice_clears_nothing_the_second_time() {
        let mut s = compacting_session(8, 150_000);
        let plan = s.plan_microcompact().expect("plan");
        assert!(s.apply_microcompact(&plan, 30_000).is_some());
        let snapshot = s.messages.clone();
        assert_eq!(s.apply_microcompact(&plan, 30_000), None);
        assert_eq!(s.messages, snapshot);
        assert_eq!(
            s.last_input_tokens, 120_000,
            "the estimate is not lowered twice"
        );
    }

    #[test]
    fn a_plan_survives_messages_added_after_it_was_made() {
        let mut s = compacting_session(8, 150_000);
        let plan = s.plan_microcompact().expect("plan");
        // Another turn lands between planning and applying.
        exchange(&mut s, "late", "Read");
        let out = s.apply_microcompact(&plan, 30_000).expect("applies");
        assert_eq!(out.cleared_tool_results, 3);
        assert_eq!(result_content(&s, "late"), json!("output of late"));
    }

    #[test]
    fn a_session_over_its_window_passes_preflight_after_microcompact() {
        let mut s = compacting_session(8, 150_000);
        let window = models::context_window(&s.model) as u64;
        // The last response already filled the window.
        s.last_input_tokens = window + 100;
        assert!(s.preflight_check().is_err(), "refused before relief");

        // Clearing old tool results that upstream counted at 30,000 tokens
        // makes room.
        let plan = s.plan_microcompact().expect("plan");
        s.apply_microcompact(&plan, 30_000).expect("applies");
        assert!(
            s.preflight_check().is_ok(),
            "relieved by clearing old results"
        );
    }

    #[test]
    fn preflight_does_not_count_max_tokens_against_the_window() {
        // 148,816 input tokens on a 200k-window model: the server accepted this
        // with max_tokens 64,000 (input + max_tokens = 212,816 > 200,000).
        let mut s = ProxySession::new(
            "s".into(),
            SessionOptions {
                model: Some("claude-haiku-4-5".to_string()),
                ..Default::default()
            },
            3600,
        );
        assert_eq!(models::context_window(&s.model), 200_000);
        s.last_input_tokens = 148_816;
        s.last_output_tokens = 4;
        assert!(s.preflight_check().is_ok());

        // Only a context that already fills the window is refused.
        s.last_input_tokens = 200_000;
        s.last_output_tokens = 0;
        assert!(s.preflight_check().is_err());
        // A session with no usage data yet is never refused.
        s.last_input_tokens = 0;
        assert!(s.preflight_check().is_ok());
    }
}
