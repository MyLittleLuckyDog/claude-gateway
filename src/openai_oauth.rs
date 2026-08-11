use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use crate::error::GatewayError;

const TOKEN_TTL_SECS: u64 = 50 * 60; // 50 minutes
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_SCOPE: &str = "openid profile email offline_access";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AuthFile {
    tokens: AuthTokens,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_refresh: Option<String>,
}

pub struct OpenAiOAuthState {
    pub client: reqwest::Client,
    pub auth_file_path: PathBuf,
    pub tokens: RwLock<AuthTokens>,
    pub last_refresh: RwLock<SystemTime>,
    pub base_url: String,
    pub token_url: String,
    pub client_id: String,
    pub total_requests: AtomicU64,
    refresh_lock: Mutex<()>,
}

impl OpenAiOAuthState {
    pub async fn from_auth_file() -> Option<Arc<Self>> {
        let path = resolve_auth_file_path()?;
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        let file: AuthFile = serde_json::from_str(&content).ok()?;

        if file.tokens.access_token.is_empty() || file.tokens.refresh_token.is_empty() {
            return None;
        }

        let last_refresh = parse_last_refresh(file.last_refresh.as_deref());

        let base_url = std::env::var("OPENAI_OAUTH_BASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let token_url = std::env::var("OPENAI_OAUTH_TOKEN_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string());

        let client_id = std::env::var("CHATGPT_LOCAL_CLIENT_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Some(Arc::new(Self {
            client,
            auth_file_path: path,
            tokens: RwLock::new(file.tokens),
            last_refresh: RwLock::new(last_refresh),
            base_url,
            token_url,
            client_id,
            total_requests: AtomicU64::new(0),
            refresh_lock: Mutex::new(()),
        }))
    }

    pub async fn ensure_fresh_token(&self) -> Result<AuthTokens, GatewayError> {
        // Fast path: most requests will find a fresh token here without contention
        let needs_refresh = {
            let lr = self.last_refresh.read().await;
            lr.elapsed().unwrap_or(Duration::MAX) >= Duration::from_secs(TOKEN_TTL_SECS)
        };

        if needs_refresh {
            // Serialize refresh: only one caller does the actual network call.
            // Re-check freshness inside the lock — a prior waiter may have already refreshed.
            let _guard = self.refresh_lock.lock().await;
            let still_stale = {
                let lr = self.last_refresh.read().await;
                lr.elapsed().unwrap_or(Duration::MAX) >= Duration::from_secs(TOKEN_TTL_SECS)
            };
            if still_stale {
                self.refresh_tokens().await?;
            }
        }

        Ok(self.tokens.read().await.clone())
    }

    async fn refresh_tokens(&self) -> Result<(), GatewayError> {
        let refresh_token = self.tokens.read().await.refresh_token.clone();

        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": self.client_id,
            "scope": OAUTH_SCOPE,
        });

        let resp = self
            .client
            .post(&self.token_url)
            .header(CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| GatewayError::CliConnection(format!("OAuth token refresh failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Internal(format!(
                "OAuth token refresh returned {status}: {text}"
            )));
        }

        let json: Value = resp.json().await.map_err(|e| {
            GatewayError::Internal(format!("OAuth refresh response decode failed: {e}"))
        })?;

        let new_access = json["access_token"]
            .as_str()
            .ok_or_else(|| GatewayError::Internal("OAuth refresh: missing access_token".into()))?
            .to_string();

        let new_refresh = json["refresh_token"]
            .as_str()
            .unwrap_or(&refresh_token)
            .to_string();

        let new_id_token = json["id_token"].as_str().map(|s| s.to_string());

        // Derive account_id from id_token JWT claims if available, otherwise keep existing
        let account_id = {
            let current = self.tokens.read().await;
            new_id_token
                .as_ref()
                .and_then(|t| extract_account_id_from_jwt(t))
                .unwrap_or_else(|| current.account_id.clone())
        };

        let now = SystemTime::now();
        let now_iso = system_time_to_iso8601(now);

        let new_tokens = AuthTokens {
            access_token: new_access,
            refresh_token: new_refresh,
            account_id,
            id_token: new_id_token,
        };

        // Write back to auth.json — merge into existing file to preserve unknown fields
        // (e.g. auth_mode, OPENAI_API_KEY written by the Codex CLI)
        let write_result = async {
            let existing = tokio::fs::read_to_string(&self.auth_file_path).await?;
            let mut root: Value =
                serde_json::from_str(&existing).unwrap_or_else(|_| serde_json::json!({}));
            root["tokens"] = serde_json::to_value(&new_tokens)?;
            root["last_refresh"] = Value::String(now_iso);
            let content = serde_json::to_string_pretty(&root)?;
            tokio::fs::write(&self.auth_file_path, content).await?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        if let Err(e) = write_result {
            tracing::warn!("Failed to write refreshed tokens to auth.json: {e}");
        }

        *self.tokens.write().await = new_tokens;
        *self.last_refresh.write().await = now;

        tracing::info!("OpenAI OAuth tokens refreshed");
        Ok(())
    }
}

/// Parameters the ChatGPT Codex `/responses` backend rejects outright with
/// `HTTP 400 "Unsupported parameter: ..."` (verified against the live backend).
/// They are stripped from every upstream request body so a caller that sets
/// them does not break the whole request.
const CODEX_UNSUPPORTED_PARAMS: &[&str] = &[
    "max_output_tokens",
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "metadata",
];

pub fn normalize_codex_body(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.entry("instructions")
            .or_insert_with(|| Value::String(String::new()));
        obj.entry("store").or_insert(Value::Bool(false));
        obj.entry("stream").or_insert(Value::Bool(true));

        // Strip backend-unsupported params. Surface what we drop so a caller
        // is not silently surprised (e.g. an ignored output-token cap).
        let dropped: Vec<&str> = CODEX_UNSUPPORTED_PARAMS
            .iter()
            .copied()
            .filter(|key| obj.remove(*key).is_some())
            .collect();
        if !dropped.is_empty() {
            tracing::warn!(
                "Stripped Codex-unsupported parameter(s) from upstream request: {}",
                dropped.join(", ")
            );
        }
    }
    normalize_reasoning_effort(&mut body);
    body
}

fn normalize_reasoning_effort(body: &mut Value) {
    let Some(reasoning) = body
        .as_object_mut()
        .and_then(|obj| obj.get_mut("reasoning"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    if reasoning.get("effort").and_then(Value::as_str) == Some("minimal") {
        reasoning.insert("effort".to_string(), Value::String("low".to_string()));
    }
}

pub fn oauth_headers(tokens: &AuthTokens) -> Result<HeaderMap, GatewayError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", tokens.access_token))
            .map_err(|e| GatewayError::Internal(format!("invalid access_token header: {e}")))?,
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(&tokens.account_id)
            .map_err(|e| GatewayError::Internal(format!("invalid account_id header: {e}")))?,
    );
    headers.insert(
        "OpenAI-Beta",
        HeaderValue::from_static("responses=experimental"),
    );
    Ok(headers)
}

pub async fn responses_sync(
    state: &OpenAiOAuthState,
    body: Value,
) -> Result<(Value, u16), GatewayError> {
    let resp = responses_raw(state, body).await?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Ok((decode_error_body(resp).await, status));
    }

    let acc = collect_response_sse(resp).await?;
    Ok((acc.to_responses_json(), status))
}

pub async fn responses_stream(
    state: &OpenAiOAuthState,
    body: Value,
) -> Result<(reqwest::Response, u16), GatewayError> {
    let resp = responses_raw(state, body).await?;
    let status = resp.status().as_u16();
    Ok((resp, status))
}

async fn responses_raw(
    state: &OpenAiOAuthState,
    body: Value,
) -> Result<reqwest::Response, GatewayError> {
    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let tokens = state.ensure_fresh_token().await?;
    let headers = oauth_headers(&tokens)?;
    let url = format!("{}/responses", state.base_url.trim_end_matches('/'));

    let resp = state
        .client
        .post(&url)
        .headers(headers)
        .json(&body)
        .send()
        .await
        .map_err(|e| GatewayError::CliConnection(format!("OAuth streaming request failed: {e}")))?;
    Ok(resp)
}

pub async fn chat_completions_sync(
    state: &OpenAiOAuthState,
    body: Value,
) -> Result<(Value, u16), GatewayError> {
    let request_model = body["model"].as_str().unwrap_or("gpt-5.4-mini").to_string();
    let resp_body = chat_to_responses_body(body, false)?;
    let resp = responses_raw(state, resp_body).await?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Ok((decode_error_body(resp).await, status));
    }

    let acc = collect_response_sse(resp).await?;
    Ok((acc.to_chat_json(&request_model), status))
}

pub async fn chat_completions_stream(
    state: &OpenAiOAuthState,
    body: Value,
) -> Result<(reqwest::Response, u16), GatewayError> {
    let resp_body = chat_to_responses_body(body, true)?;
    let resp = responses_raw(state, resp_body).await?;
    let status = resp.status().as_u16();
    Ok((resp, status))
}

pub async fn models(state: &OpenAiOAuthState) -> Result<(Value, u16), GatewayError> {
    if let Some(cached) = models_from_codex_cache().await {
        return Ok((cached, 200));
    }

    state.total_requests.fetch_add(1, Ordering::Relaxed);
    let tokens = state.ensure_fresh_token().await?;
    let headers = oauth_headers(&tokens)?;
    let url = format!(
        "{}/models?client_version={}",
        state.base_url.trim_end_matches('/'),
        std::env::var("CODEX_CLIENT_VERSION").unwrap_or_else(|_| "0.128.0".to_string())
    );

    let resp = state
        .client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .map_err(|e| GatewayError::CliConnection(format!("OAuth models request failed: {e}")))?;

    let status = resp.status().as_u16();
    let json = resp
        .json::<Value>()
        .await
        .map_err(|e| GatewayError::Internal(format!("OAuth models JSON decode failed: {e}")))?;
    Ok((json, status))
}

pub fn chat_sse_body(resp: reqwest::Response, request_model: String) -> axum::body::Body {
    let mut upstream = resp.bytes_stream();
    let stream = async_stream::stream! {
        let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let created = unix_now();
        let mut buf = String::new();

        while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    let payload = json!({"error":{"type":"upstream_error","message":e.to_string()}});
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(format!("event: error\ndata: {}\n\n", payload)));
                    return;
                }
            };

            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(idx) = buf.find("\n\n") {
                let block = buf[..idx].to_string();
                buf = buf[idx + 2..].to_string();
                if let Some(data) = sse_data(&block) {
                    let Ok(value) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };

                    match value["type"].as_str().unwrap_or_default() {
                        "response.output_text.delta" => {
                            let delta = value["delta"].as_str().unwrap_or_default();
                            let chunk = json!({
                                "id": id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": request_model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {"content": delta},
                                    "finish_reason": null
                                }]
                            });
                            yield Ok(Bytes::from(format!("data: {}\n\n", chunk)));
                        }
                        "response.output_item.added" => {
                            if value["item"]["type"].as_str() == Some("function_call") {
                                let tool_call = response_function_item_to_chat_delta(&value["item"]);
                                let chunk = json!({
                                    "id": id,
                                    "object": "chat.completion.chunk",
                                    "created": created,
                                    "model": request_model,
                                    "choices": [{
                                        "index": 0,
                                        "delta": {"tool_calls": [tool_call]},
                                        "finish_reason": null
                                    }]
                                });
                                yield Ok(Bytes::from(format!("data: {}\n\n", chunk)));
                            }
                        }
                        "response.completed" => {
                            let chunk = json!({
                                "id": id,
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": request_model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {},
                                    "finish_reason": "stop"
                                }],
                                "usage": chat_usage(value["response"]["usage"].clone())
                            });
                            yield Ok(Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", chunk)));
                        }
                        "error" => {
                            yield Ok(Bytes::from(format!("event: error\ndata: {}\n\n", value)));
                        }
                        _ => {}
                    }
                }
            }
        }
    };
    axum::body::Body::from_stream(stream)
}

#[derive(Debug, Default)]
struct ResponseAccumulator {
    id: Option<String>,
    created: Option<i64>,
    model: Option<String>,
    message_id: Option<String>,
    text: String,
    usage: Option<Value>,
    response: Option<Value>,
    tool_calls: Vec<Value>,
    response_function_calls: Vec<Value>,
}

impl ResponseAccumulator {
    fn apply_event(&mut self, value: &Value) {
        match value["type"].as_str().unwrap_or_default() {
            "response.created" | "response.in_progress" | "response.completed" => {
                let response = &value["response"];
                self.id = response["id"]
                    .as_str()
                    .map(str::to_string)
                    .or(self.id.take());
                self.model = response["model"]
                    .as_str()
                    .map(str::to_string)
                    .or(self.model.take());
                self.created = response["created_at"].as_i64().or(self.created);
                if !response["usage"].is_null() {
                    self.usage = Some(response["usage"].clone());
                }
                if value["type"].as_str() == Some("response.completed") {
                    self.response = Some(response.clone());
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = value["delta"].as_str() {
                    self.text.push_str(delta);
                }
            }
            "response.output_text.done" => {
                if let Some(text) = value["text"].as_str() {
                    self.text = text.to_string();
                }
            }
            "response.output_item.added" => {
                let item = &value["item"];
                if item["type"].as_str() == Some("message") {
                    self.message_id = item["id"].as_str().map(str::to_string);
                }
            }
            "response.output_item.done" => {
                let item = &value["item"];
                match item["type"].as_str().unwrap_or_default() {
                    "message" => {
                        self.message_id = item["id"]
                            .as_str()
                            .map(str::to_string)
                            .or(self.message_id.take());
                        if self.text.is_empty() {
                            if let Some(text) = extract_text_from_response_message(item) {
                                self.text = text;
                            }
                        }
                    }
                    "function_call" => {
                        self.tool_calls
                            .push(response_function_item_to_chat_message(item));
                        self.response_function_calls
                            .push(response_function_item_to_response_output(item));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn to_responses_json(&self) -> Value {
        let mut response = self.response.clone().unwrap_or_else(|| {
            json!({
                "id": self.id.clone().unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4())),
                "object": "response",
                "created_at": self.created.unwrap_or_else(unix_now),
                "status": "completed",
                "model": self.model.clone().unwrap_or_else(|| "gpt-5.4-mini".to_string()),
                "output": [],
                "usage": self.usage.clone().unwrap_or(Value::Null),
            })
        });

        if let Some(obj) = response.as_object_mut() {
            obj.insert("output_text".to_string(), Value::String(self.text.clone()));
            let output_missing_or_empty = obj
                .get("output")
                .and_then(Value::as_array)
                .is_none_or(|a| a.is_empty());
            if output_missing_or_empty {
                obj.insert(
                    "output".to_string(),
                    Value::Array(self.to_responses_output()),
                );
            } else if let Some(output) = obj.get_mut("output").and_then(Value::as_array_mut) {
                normalize_response_output_items(output);
            }
        }
        response
    }

    fn to_responses_output(&self) -> Vec<Value> {
        let mut output = Vec::new();
        if !self.text.is_empty() {
            output.push(json!({
                "id": self.message_id.clone().unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4())),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": self.text,
                    "annotations": [],
                    "logprobs": []
                }]
            }));
        }
        output.extend(self.response_function_calls.iter().cloned());
        output
    }

    fn to_chat_json(&self, request_model: &str) -> Value {
        let message = if self.tool_calls.is_empty() {
            json!({"role": "assistant", "content": self.text})
        } else {
            json!({"role": "assistant", "content": self.text, "tool_calls": self.tool_calls})
        };

        json!({
            "id": self.id.clone().unwrap_or_else(|| format!("chatcmpl-{}", uuid::Uuid::new_v4())),
            "object": "chat.completion",
            "created": self.created.unwrap_or_else(unix_now),
            "model": self.model.clone().unwrap_or_else(|| request_model.to_string()),
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": if self.tool_calls.is_empty() { "stop" } else { "tool_calls" }
            }],
            "usage": chat_usage(self.usage.clone().unwrap_or(Value::Null))
        })
    }
}

async fn collect_response_sse(
    resp: reqwest::Response,
) -> Result<ResponseAccumulator, GatewayError> {
    let text = resp
        .text()
        .await
        .map_err(|e| GatewayError::Internal(format!("OAuth responses body read failed: {e}")))?;
    let mut acc = ResponseAccumulator::default();
    for block in text.split("\n\n") {
        if let Some(data) = sse_data(block) {
            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                acc.apply_event(&value);
            }
        }
    }
    Ok(acc)
}

async fn decode_error_body(resp: reqwest::Response) -> Value {
    match resp.json::<Value>().await {
        Ok(value) => value,
        Err(e) => json!({"error": {"type": "upstream_error", "message": e.to_string()}}),
    }
}

fn sse_data(block: &str) -> Option<String> {
    let data = block
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() || data == "[DONE]" {
        None
    } else {
        Some(data)
    }
}

/// OpenAI Chat Completions parameters that have no effect once a request is
/// translated for the Codex `/responses` backend — sampling controls and
/// output-token caps the backend does not honor. The translation simply omits
/// them; this list exists only to log what the caller asked for and lost.
const CHAT_IGNORED_PARAMS: &[&str] = &[
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "n",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "stop",
    "metadata",
];

fn warn_ignored_chat_params(obj: &serde_json::Map<String, Value>) {
    let ignored: Vec<&str> = CHAT_IGNORED_PARAMS
        .iter()
        .copied()
        .filter(|key| obj.contains_key(*key))
        .collect();
    if !ignored.is_empty() {
        tracing::warn!(
            "chat/completions: ignoring parameter(s) not supported by the Codex backend: {}",
            ignored.join(", ")
        );
    }
}

pub fn chat_to_responses_body(body: Value, _stream: bool) -> Result<Value, GatewayError> {
    let obj = body
        .as_object()
        .ok_or_else(|| GatewayError::Internal("Request body must be a JSON object".into()))?;
    warn_ignored_chat_params(obj);
    let model = obj
        .get("model")
        .cloned()
        .unwrap_or_else(|| Value::String("gpt-5.4-mini".to_string()));
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::Internal("Missing required field: messages".into()))?;

    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("user");
        if matches!(role, "system" | "developer") {
            let text = chat_content_to_text(&msg["content"]);
            if !text.is_empty() {
                instructions.push(text);
            }
            continue;
        }

        if role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": msg["tool_call_id"].as_str().unwrap_or_default(),
                "output": chat_content_to_text(&msg["content"])
            }));
            continue;
        }

        if role == "assistant" {
            let text = chat_content_to_text(&msg["content"]);
            if !text.is_empty() {
                input.push(json!({
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]
                }));
            }
            if let Some(tool_calls) = msg["tool_calls"].as_array() {
                input.extend(tool_calls.iter().map(chat_tool_call_to_response_item));
            }
            continue;
        }

        input.push(json!({
            "role": role,
            "content": [{"type": "input_text", "text": chat_content_to_text(&msg["content"])}]
        }));
    }

    let mut out = json!({
        "model": model,
        "instructions": instructions.join("\n\n"),
        "input": input,
        "stream": true,
        "store": false,
    });

    if let Some(out_obj) = out.as_object_mut() {
        // Sampling/limit params (temperature, top_p, metadata, …) are
        // intentionally not forwarded — the Codex backend rejects them.
        // See CHAT_IGNORED_PARAMS / CODEX_UNSUPPORTED_PARAMS.
        if let Some(tool_choice) = obj.get("tool_choice") {
            out_obj.insert(
                "tool_choice".to_string(),
                chat_tool_choice_to_response(tool_choice),
            );
        }
        if let Some(reasoning) = obj.get("reasoning") {
            out_obj.insert("reasoning".to_string(), reasoning.clone());
        }
        if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
            out_obj.insert(
                "tools".to_string(),
                Value::Array(tools.iter().map(chat_tool_to_response_tool).collect()),
            );
        }
        // The Codex OAuth backend requires streaming; non-stream callers are
        // adapted by collecting the SSE response above.
        out_obj.insert("stream".to_string(), Value::Bool(true));
    }

    Ok(normalize_codex_body(out))
}

fn chat_content_to_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part["text"]
                        .as_str()
                        .or_else(|| part["content"].as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn chat_tool_choice_to_response(tool_choice: &Value) -> Value {
    if let Some(obj) = tool_choice.as_object() {
        if obj.get("type").and_then(Value::as_str) == Some("function") {
            if let Some(name) = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                return json!({"type": "function", "name": name});
            }
        }
    }
    tool_choice.clone()
}

fn chat_tool_to_response_tool(tool: &Value) -> Value {
    if tool["type"].as_str() == Some("function") {
        let function = &tool["function"];
        json!({
            "type": "function",
            "name": function["name"].clone(),
            "description": function["description"].clone(),
            "parameters": function["parameters"].clone()
        })
    } else {
        tool.clone()
    }
}

fn chat_tool_call_to_response_item(tool_call: &Value) -> Value {
    let function = &tool_call["function"];
    json!({
        "type": "function_call",
        "call_id": tool_call["id"].as_str().unwrap_or_default(),
        "name": function["name"].as_str().unwrap_or_default(),
        "arguments": function["arguments"].as_str().unwrap_or_default()
    })
}

fn extract_text_from_response_message(item: &Value) -> Option<String> {
    item["content"]
        .as_array()
        .map(|content| {
            content
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
}

fn response_function_item_to_chat_message(item: &Value) -> Value {
    json!({
        "id": item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default(),
        "type": "function",
        "function": {
            "name": item["name"].as_str().unwrap_or_default(),
            "arguments": item["arguments"].as_str().unwrap_or_default()
        }
    })
}

fn response_function_item_to_response_output(item: &Value) -> Value {
    json!({
        "id": item["id"].as_str().unwrap_or_else(|| item["call_id"].as_str().unwrap_or_default()),
        "type": "function_call",
        "status": item["status"].as_str().unwrap_or("completed"),
        "call_id": item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default(),
        "name": item["name"].as_str().unwrap_or_default(),
        "arguments": item["arguments"].as_str().unwrap_or_default()
    })
}

fn response_function_item_to_chat_delta(item: &Value) -> Value {
    json!({
        "index": 0,
        "id": item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default(),
        "type": "function",
        "function": {
            "name": item["name"].as_str().unwrap_or_default(),
            "arguments": item["arguments"].as_str().unwrap_or_default()
        }
    })
}

fn normalize_response_output_items(output: &mut [Value]) {
    for item in output {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        match obj.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message" => {
                obj.entry("id".to_string())
                    .or_insert_with(|| Value::String(format!("msg_{}", uuid::Uuid::new_v4())));
                obj.entry("status".to_string())
                    .or_insert_with(|| Value::String("completed".to_string()));
                obj.entry("role".to_string())
                    .or_insert_with(|| Value::String("assistant".to_string()));
                if let Some(content) = obj.get_mut("content").and_then(Value::as_array_mut) {
                    for part in content {
                        if let Some(part_obj) = part.as_object_mut() {
                            part_obj
                                .entry("annotations".to_string())
                                .or_insert_with(|| Value::Array(Vec::new()));
                            part_obj
                                .entry("logprobs".to_string())
                                .or_insert_with(|| Value::Array(Vec::new()));
                        }
                    }
                }
            }
            "function_call" => {
                obj.entry("id".to_string())
                    .or_insert_with(|| Value::String(format!("fc_{}", uuid::Uuid::new_v4())));
                obj.entry("status".to_string())
                    .or_insert_with(|| Value::String("completed".to_string()));
            }
            _ => {}
        }
    }
}

fn chat_usage(usage: Value) -> Value {
    if usage.is_null() {
        return Value::Null;
    }
    json!({
        "prompt_tokens": usage["input_tokens"].as_u64().unwrap_or(0),
        "completion_tokens": usage["output_tokens"].as_u64().unwrap_or(0),
        "total_tokens": usage["total_tokens"].as_u64().unwrap_or(0),
        "prompt_tokens_details": usage["input_tokens_details"].clone(),
        "completion_tokens_details": usage["output_tokens_details"].clone()
    })
}

async fn models_from_codex_cache() -> Option<Value> {
    let home = dirs_home()?;
    let path = home.join(".codex").join("models_cache.json");
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let cache: Value = serde_json::from_str(&content).ok()?;
    let models = cache["models"].as_array()?;
    let data = models
        .iter()
        .filter(|m| m["supported_in_api"].as_bool().unwrap_or(false))
        .map(|m| {
            json!({
                "id": m["slug"].as_str().unwrap_or_default(),
                "object": "model",
                "created": 0,
                "owned_by": "openai",
                "display_name": m["display_name"].clone(),
                "description": m["description"].clone()
            })
        })
        .collect::<Vec<_>>();
    Some(json!({"object": "list", "data": data}))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ── Helpers ───────────────────────────────────────────────────────

fn resolve_auth_file_path() -> Option<PathBuf> {
    let home = dirs_home();

    let candidates: Vec<PathBuf> = [
        std::env::var("CHATGPT_LOCAL_HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("auth.json")),
        std::env::var("CODEX_HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("auth.json")),
        home.as_ref()
            .map(|h| h.join(".chatgpt-local").join("auth.json")),
        home.as_ref().map(|h| h.join(".codex").join("auth.json")),
    ]
    .into_iter()
    .flatten()
    .collect();

    candidates.into_iter().find(|p| p.exists())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn parse_last_refresh(s: Option<&str>) -> SystemTime {
    let Some(s) = s else {
        return UNIX_EPOCH;
    };

    // Try ISO 8601 via chrono
    if let Ok(dt) = s.parse::<DateTime<Utc>>() {
        let secs = dt.timestamp();
        if secs > 0 {
            return UNIX_EPOCH + Duration::from_secs(secs as u64);
        }
    }

    // Try plain Unix timestamp
    if let Ok(secs) = s.parse::<u64>() {
        return UNIX_EPOCH + Duration::from_secs(secs);
    }

    UNIX_EPOCH
}

fn system_time_to_iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339()
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    // Base64url decode the payload (second part)
    let padded = {
        let part = parts[1];
        let mut s = part.replace('-', "+").replace('_', "/");
        while !s.len().is_multiple_of(4) {
            s.push('=');
        }
        s
    };

    let decoded = base64_decode(&padded)?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims["https://api.openai.com/auth.chatgpt_account_id"]
        .as_str()
        .map(|s| s.to_string())
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    // Simple base64 decoder without external crate
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [0u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }

    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut result = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;

    for b in bytes {
        buf = (buf << 6) | lookup[b as usize] as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            result.push((buf >> bits) as u8);
        }
    }

    Some(result)
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
// ENV_LOCK is intentionally held across awaits: it guards process-wide env vars
// for the whole body of an async test, and #[tokio::test] runs single-threaded,
// so the guard cannot block another worker.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::UNIX_EPOCH;

    // Serialize all tests that mutate environment variables — set_var is not thread-safe
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ── normalize_codex_body ─────────────────────────────────────

    #[test]
    fn normalize_sets_default_instructions_when_absent() {
        let body = serde_json::json!({"model": "gpt-4o", "input": "hi"});
        let out = normalize_codex_body(body);
        assert_eq!(
            out["instructions"],
            serde_json::Value::String(String::new())
        );
    }

    #[test]
    fn normalize_preserves_existing_instructions() {
        let body =
            serde_json::json!({"model": "gpt-4o", "input": "hi", "instructions": "be concise"});
        let out = normalize_codex_body(body);
        assert_eq!(out["instructions"], "be concise");
    }

    #[test]
    fn normalize_sets_store_false_when_absent() {
        let body = serde_json::json!({"model": "gpt-4o", "input": "hi"});
        let out = normalize_codex_body(body);
        assert_eq!(out["store"], serde_json::Value::Bool(false));
    }

    #[test]
    fn normalize_preserves_store_true_when_set() {
        let body = serde_json::json!({"model": "gpt-4o", "input": "hi", "store": true});
        let out = normalize_codex_body(body);
        assert_eq!(out["store"], serde_json::Value::Bool(true));
    }

    #[test]
    fn normalize_removes_max_output_tokens() {
        let body = serde_json::json!({"model": "gpt-4o", "input": "hi", "max_output_tokens": 1024});
        let out = normalize_codex_body(body);
        assert!(out.as_object().unwrap().get("max_output_tokens").is_none());
    }

    #[test]
    fn normalize_preserves_passthrough_fields() {
        let body = serde_json::json!({"model": "gpt-4o", "input": "hi", "user": "u1"});
        let out = normalize_codex_body(body);
        assert_eq!(out["model"], "gpt-4o");
        assert_eq!(out["input"], "hi");
        assert_eq!(out["user"], "u1");
    }

    #[test]
    fn normalize_strips_codex_unsupported_params() {
        let body = serde_json::json!({
            "model": "gpt-5.4",
            "input": "hi",
            "temperature": 0.7,
            "top_p": 0.9,
            "frequency_penalty": 0.5,
            "presence_penalty": 0.1,
            "metadata": {"k": "v"},
            "max_output_tokens": 100
        });
        let out = normalize_codex_body(body);
        let obj = out.as_object().unwrap();
        for key in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "metadata",
            "max_output_tokens",
        ] {
            assert!(obj.get(key).is_none(), "{key} should be stripped");
        }
        assert_eq!(out["model"], "gpt-5.4");
        assert_eq!(out["input"], "hi");
    }

    #[test]
    fn normalize_reasoning_minimal_to_low() {
        let body = serde_json::json!({
            "model": "gpt-5.4-mini",
            "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "reasoning": {"effort": "minimal"}
        });
        let out = normalize_codex_body(body);
        assert_eq!(out["reasoning"]["effort"], "low");
    }

    #[test]
    fn normalize_does_not_add_reasoning_when_absent() {
        let body = serde_json::json!({"model": "gpt-5.4-mini", "input": "hi"});
        let out = normalize_codex_body(body);
        assert!(out.as_object().unwrap().get("reasoning").is_none());
    }

    #[test]
    fn normalize_non_object_passthrough() {
        let body = serde_json::json!("not an object");
        let out = normalize_codex_body(body.clone());
        assert_eq!(out, body);
    }

    // ── Chat Completions adapter ────────────────────────────────

    #[test]
    fn chat_to_responses_body_extracts_instructions_and_tools() {
        let body = serde_json::json!({
            "model": "gpt-5.4-mini",
            "messages": [
                {"role": "system", "content": "You are concise."},
                {"role": "user", "content": "Hello"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "extract_heading",
                    "description": "Extract the page heading",
                    "parameters": {
                        "type": "object",
                        "properties": {"heading": {"type": "string"}},
                        "required": ["heading"]
                    }
                }
            }],
            "tool_choice": "auto",
            "reasoning": {"effort": "minimal"}
        });

        let out = chat_to_responses_body(body, false).unwrap();
        assert_eq!(out["model"], "gpt-5.4-mini");
        assert_eq!(out["instructions"], "You are concise.");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(out["input"][0]["content"][0]["text"], "Hello");
        assert_eq!(out["tools"][0]["type"], "function");
        assert_eq!(out["tools"][0]["name"], "extract_heading");
        assert_eq!(out["tool_choice"], "auto");
        assert_eq!(out["reasoning"]["effort"], "low");
        assert_eq!(out["stream"], true);
    }

    #[test]
    fn chat_to_responses_body_maps_assistant_tool_calls_and_tool_results() {
        let body = serde_json::json!({
            "model": "gpt-5.4-mini",
            "messages": [
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_123",
                        "type": "function",
                        "function": {
                            "name": "get_page_text",
                            "arguments": "{\"selector\":\"h1\"}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call_123",
                    "content": "{\"text\":\"Example Domain\"}"
                },
                {"role": "user", "content": "Summarize it"}
            ]
        });

        let out = chat_to_responses_body(body, false).unwrap();
        assert_eq!(out["input"][0]["type"], "function_call");
        assert_eq!(out["input"][0]["call_id"], "call_123");
        assert_eq!(out["input"][0]["name"], "get_page_text");
        assert_eq!(out["input"][0]["arguments"], "{\"selector\":\"h1\"}");
        assert_eq!(out["input"][1]["type"], "function_call_output");
        assert_eq!(out["input"][1]["call_id"], "call_123");
        assert_eq!(out["input"][1]["output"], "{\"text\":\"Example Domain\"}");
        assert_eq!(out["input"][2]["role"], "user");
        assert_eq!(out["input"][2]["content"][0]["text"], "Summarize it");
    }

    #[test]
    fn chat_to_responses_body_maps_object_tool_choice() {
        let body = serde_json::json!({
            "model": "gpt-5.4-mini",
            "messages": [{"role": "user", "content": "Call the tool"}],
            "tool_choice": {
                "type": "function",
                "function": {"name": "extract_heading"}
            }
        });

        let out = chat_to_responses_body(body, false).unwrap();
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["name"], "extract_heading");
        assert!(out["tool_choice"].get("function").is_none());
    }

    #[test]
    fn response_accumulator_maps_function_call_to_chat_tool_calls() {
        let mut acc = ResponseAccumulator::default();
        acc.apply_event(&serde_json::json!({
            "type": "response.created",
            "response": {
                "id": "resp_123",
                "created_at": 1778060000,
                "model": "gpt-5.4-mini",
                "usage": null
            }
        }));
        acc.apply_event(&serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "call_id": "call_123",
                "name": "extract_heading",
                "arguments": "{\"heading\":\"Example Domain\"}"
            }
        }));

        let out = acc.to_chat_json("gpt-5.4-mini");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_123"
        );
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "extract_heading"
        );

        let responses = acc.to_responses_json();
        assert_eq!(responses["output"][0]["type"], "function_call");
        assert_eq!(responses["output"][0]["status"], "completed");
        assert_eq!(responses["output"][0]["call_id"], "call_123");
        assert_eq!(responses["output"][0]["name"], "extract_heading");
        assert_eq!(
            responses["output"][0]["arguments"],
            "{\"heading\":\"Example Domain\"}"
        );
    }

    #[test]
    fn response_accumulator_synthesizes_responses_message_schema() {
        let mut acc = ResponseAccumulator::default();
        acc.apply_event(&serde_json::json!({
            "type": "response.created",
            "response": {
                "id": "resp_123",
                "created_at": 1778060000,
                "model": "gpt-5.4-mini",
                "output": [],
                "usage": null
            }
        }));
        acc.apply_event(&serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "id": "msg_123",
                "type": "message",
                "status": "in_progress",
                "role": "assistant"
            }
        }));
        acc.apply_event(&serde_json::json!({
            "type": "response.output_text.done",
            "text": "Example Domain"
        }));

        let out = acc.to_responses_json();
        assert_eq!(out["output_text"], "Example Domain");
        assert_eq!(out["output"][0]["id"], "msg_123");
        assert_eq!(out["output"][0]["type"], "message");
        assert_eq!(out["output"][0]["status"], "completed");
        assert_eq!(out["output"][0]["role"], "assistant");
        assert_eq!(out["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(out["output"][0]["content"][0]["text"], "Example Domain");
        assert_eq!(
            out["output"][0]["content"][0]["annotations"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            out["output"][0]["content"][0]["logprobs"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn response_accumulator_normalizes_existing_output_schema() {
        let mut acc = ResponseAccumulator::default();
        acc.apply_event(&serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_123",
                "created_at": 1778060000,
                "model": "gpt-5.4-mini",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hi"}]
                }],
                "usage": null
            }
        }));

        let out = acc.to_responses_json();
        assert!(out["output"][0]["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(out["output"][0]["status"], "completed");
        assert!(out["output"][0]["content"][0]["annotations"].is_array());
        assert!(out["output"][0]["content"][0]["logprobs"].is_array());
    }

    // ── parse_last_refresh ───────────────────────────────────────

    #[test]
    fn parse_none_returns_epoch() {
        let t = parse_last_refresh(None);
        assert_eq!(t, UNIX_EPOCH);
    }

    #[test]
    fn parse_empty_string_returns_epoch() {
        let t = parse_last_refresh(Some(""));
        assert_eq!(t, UNIX_EPOCH);
    }

    #[test]
    fn parse_invalid_returns_epoch() {
        let t = parse_last_refresh(Some("not-a-date"));
        assert_eq!(t, UNIX_EPOCH);
    }

    #[test]
    fn parse_iso8601_returns_correct_time() {
        // 2024-01-01T00:00:00Z = Unix 1704067200
        let t = parse_last_refresh(Some("2024-01-01T00:00:00Z"));
        let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1704067200);
    }

    #[test]
    fn parse_unix_u64_returns_correct_time() {
        let t = parse_last_refresh(Some("1704067200"));
        let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1704067200);
    }

    #[test]
    fn parse_recent_iso8601_not_expired() {
        // A recent timestamp should not be expired (< TOKEN_TTL_SECS ago)
        let now_iso = system_time_to_iso8601(SystemTime::now());
        let t = parse_last_refresh(Some(&now_iso));
        let elapsed = t.elapsed().unwrap_or(Duration::MAX);
        assert!(elapsed < Duration::from_secs(TOKEN_TTL_SECS));
    }

    // ── base64_decode ────────────────────────────────────────────

    #[test]
    fn base64_decode_hello() {
        // "hello" in base64 = "aGVsbG8="
        let decoded = base64_decode("aGVsbG8=").unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn base64_decode_empty() {
        let decoded = base64_decode("").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn base64_decode_roundtrip_json() {
        let json = r#"{"sub":"user123","name":"Alex"}"#;
        let encoded = base64url_encode(json.as_bytes());
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, json.as_bytes());
    }

    // ── extract_account_id_from_jwt ──────────────────────────────

    #[test]
    fn extract_account_id_returns_none_for_invalid_jwt() {
        assert!(extract_account_id_from_jwt("notajwt").is_none());
        let _ = extract_account_id_from_jwt("a.b"); // must not panic
    }

    #[test]
    fn extract_account_id_returns_none_when_claim_absent() {
        // JWT with payload {"sub": "user123"} — no chatgpt_account_id
        let payload = r#"{"sub":"user123"}"#;
        let encoded = base64url_encode(payload.as_bytes());
        let fake_jwt = format!("header.{encoded}.signature");
        let result = extract_account_id_from_jwt(&fake_jwt);
        assert!(result.is_none());
    }

    #[test]
    fn extract_account_id_returns_value_when_present() {
        let payload =
            r#"{"sub":"user123","https://api.openai.com/auth.chatgpt_account_id":"acct_abc123"}"#;
        let encoded = base64url_encode(payload.as_bytes());
        let fake_jwt = format!("header.{encoded}.signature");
        let result = extract_account_id_from_jwt(&fake_jwt);
        assert_eq!(result, Some("acct_abc123".to_string()));
    }

    // ── from_auth_file (integration) ────────────────────────────

    #[tokio::test]
    async fn from_auth_file_loads_valid_file() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let content = serde_json::json!({
            "tokens": {
                "access_token": "test-access-token",
                "refresh_token": "test-refresh-token",
                "account_id": "acct_test123"
            },
            "last_refresh": "2024-01-01T00:00:00Z"
        });
        tokio::fs::write(&path, serde_json::to_string(&content).unwrap())
            .await
            .unwrap();

        unsafe {
            std::env::set_var("CODEX_HOME", dir.path().to_str().unwrap());
        }
        let state = OpenAiOAuthState::from_auth_file().await;
        unsafe {
            std::env::remove_var("CODEX_HOME");
        }

        assert!(state.is_some());
        let s = state.unwrap();
        let tokens = s.tokens.read().await;
        assert_eq!(tokens.access_token, "test-access-token");
        assert_eq!(tokens.refresh_token, "test-refresh-token");
        assert_eq!(tokens.account_id, "acct_test123");
    }

    #[tokio::test]
    async fn from_auth_file_returns_none_for_missing_file() {
        let _env = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("CODEX_HOME", "/tmp/claude_gateway_test_nonexistent_xyz");
            std::env::set_var(
                "CHATGPT_LOCAL_HOME",
                "/tmp/claude_gateway_test_nonexistent_xyz",
            );
        }
        let state = OpenAiOAuthState::from_auth_file().await;
        unsafe {
            std::env::remove_var("CODEX_HOME");
            std::env::remove_var("CHATGPT_LOCAL_HOME");
        }
        // Real ~/.codex/auth.json may exist; test just ensures no panic.
        let _ = state;
    }

    #[tokio::test]
    async fn from_auth_file_returns_none_for_empty_tokens() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let content = serde_json::json!({
            "tokens": {
                "access_token": "",
                "refresh_token": "rt_valid",
                "account_id": "acct_123"
            }
        });
        tokio::fs::write(&path, serde_json::to_string(&content).unwrap())
            .await
            .unwrap();

        unsafe {
            std::env::set_var("CODEX_HOME", dir.path().to_str().unwrap());
        }
        let state = OpenAiOAuthState::from_auth_file().await;
        unsafe {
            std::env::remove_var("CODEX_HOME");
        }

        assert!(state.is_none());
    }

    #[tokio::test]
    async fn from_auth_file_ignores_unknown_fields() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let content = serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": "tok_abc",
                "refresh_token": "rt_abc",
                "account_id": "acct_abc",
                "id_token": "id_tok_abc"
            },
            "last_refresh": "2026-05-01T00:00:00Z"
        });
        tokio::fs::write(&path, serde_json::to_string(&content).unwrap())
            .await
            .unwrap();

        unsafe {
            std::env::set_var("CODEX_HOME", dir.path().to_str().unwrap());
        }
        let state = OpenAiOAuthState::from_auth_file().await;
        unsafe {
            std::env::remove_var("CODEX_HOME");
        }

        assert!(state.is_some());
        let s = state.unwrap();
        let tokens = s.tokens.read().await;
        assert_eq!(tokens.account_id, "acct_abc");
        assert_eq!(tokens.id_token, Some("id_tok_abc".to_string()));
    }

    // ── ensure_fresh_token (TTL logic) ───────────────────────────

    #[tokio::test]
    async fn ensure_fresh_token_returns_tokens_when_not_expired() {
        let _env = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let content = serde_json::json!({
            "tokens": {
                "access_token": "fresh-token",
                "refresh_token": "rt_valid",
                "account_id": "acct_123"
            },
            "last_refresh": system_time_to_iso8601(SystemTime::now())
        });
        tokio::fs::write(&path, serde_json::to_string_pretty(&content).unwrap())
            .await
            .unwrap();

        unsafe {
            std::env::set_var("CODEX_HOME", dir.path().to_str().unwrap());
        }
        let state = OpenAiOAuthState::from_auth_file().await.unwrap();
        unsafe {
            std::env::remove_var("CODEX_HOME");
        }

        // Token is fresh (just set last_refresh to now) — should NOT trigger refresh
        let tokens = state.ensure_fresh_token().await.unwrap();
        assert_eq!(tokens.access_token, "fresh-token");
    }

    // ── Helpers ──────────────────────────────────────────────────

    fn base64url_encode(input: &[u8]) -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut result = String::new();
        let mut i = 0;
        while i < input.len() {
            let b0 = input[i] as u32;
            let b1 = if i + 1 < input.len() {
                input[i + 1] as u32
            } else {
                0
            };
            let b2 = if i + 2 < input.len() {
                input[i + 2] as u32
            } else {
                0
            };

            result.push(alphabet[((b0 >> 2) & 0x3f) as usize] as char);
            result.push(alphabet[(((b0 << 4) | (b1 >> 4)) & 0x3f) as usize] as char);
            if i + 1 < input.len() {
                result.push(alphabet[(((b1 << 2) | (b2 >> 6)) & 0x3f) as usize] as char);
            }
            if i + 2 < input.len() {
                result.push(alphabet[(b2 & 0x3f) as usize] as char);
            }
            i += 3;
        }
        result
    }
}
