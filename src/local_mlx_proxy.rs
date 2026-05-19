use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::error::GatewayError;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_MODEL: &str = "Jiunsong/supergemma4-26b-uncensored-mlx-4bit-v2";
const DEFAULT_ALIAS: &str = "gemma";
const DEFAULT_MIN_MAX_TOKENS: u64 = 1024;

pub struct LocalMlxProxyState {
    pub client: reqwest::Client,
    pub base_url: String,
    pub model: String,
    pub alias: String,
    pub min_max_tokens: u64,
    pub total_requests: AtomicU64,
}

impl LocalMlxProxyState {
    pub fn from_env() -> Option<Arc<Self>> {
        if env_var("GEMMA_ENABLED", "LOCAL_MLX_ENABLED")
            .as_deref()
            .is_some_and(|v| matches!(v, "0" | "false" | "FALSE" | "False"))
        {
            return None;
        }

        let base_url = env_var("GEMMA_BASE_URL", "LOCAL_MLX_BASE_URL")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let model = env_var("GEMMA_MODEL", "LOCAL_MLX_MODEL")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let alias = env_var("GEMMA_ALIAS", "LOCAL_MLX_ALIAS")
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_ALIAS.to_string());
        let min_max_tokens = env_var("GEMMA_MIN_MAX_TOKENS", "LOCAL_MLX_MIN_MAX_TOKENS")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MIN_MAX_TOKENS);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Some(Arc::new(Self {
            client,
            base_url,
            model,
            alias,
            min_max_tokens,
            total_requests: AtomicU64::new(0),
        }))
    }

    fn upstream_model_for(&self, requested: Option<&str>) -> String {
        match requested {
            Some(model)
                if model == self.alias
                    || model == "gemma"
                    || model == "local-mlx"
                    || model == "mlx-local" =>
            {
                self.model.clone()
            }
            Some(model) if !model.trim().is_empty() => model.to_string(),
            _ => self.model.clone(),
        }
    }
}

fn env_var(primary: &str, fallback: &str) -> Option<String> {
    std::env::var(primary)
        .ok()
        .or_else(|| std::env::var(fallback).ok())
}

fn json_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

pub fn normalize_chat_body(state: &LocalMlxProxyState, mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        let requested_model = obj.get("model").and_then(Value::as_str);
        obj.insert(
            "model".to_string(),
            Value::String(state.upstream_model_for(requested_model)),
        );

        let requested_max_tokens = obj.get("max_tokens").and_then(Value::as_u64).unwrap_or(0);
        if requested_max_tokens < state.min_max_tokens {
            obj.insert(
                "max_tokens".to_string(),
                Value::Number(state.min_max_tokens.into()),
            );
        }
    }
    body
}

pub fn strip_mlx_channels(value: &mut Value) {
    match value {
        Value::Object(obj) => {
            obj.remove("reasoning");
            for value in obj.values_mut() {
                strip_mlx_channels(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_mlx_channels(value);
            }
        }
        _ => {}
    }
}

pub async fn chat_completions_sync(
    state: &LocalMlxProxyState,
    body: Value,
) -> Result<(Value, u16), GatewayError> {
    state.total_requests.fetch_add(1, Ordering::Relaxed);

    let body = normalize_chat_body(state, body);
    let url = format!(
        "{}/v1/chat/completions",
        state.base_url.trim_end_matches('/')
    );
    let resp = state
        .client
        .post(url)
        .headers(json_headers())
        .json(&body)
        .send()
        .await
        .map_err(|e| GatewayError::CliConnection(format!("Local MLX chat request failed: {e}")))?;

    let status = resp.status().as_u16();
    let mut json = resp.json::<Value>().await.map_err(|e| {
        GatewayError::Internal(format!("Local MLX response JSON decode failed: {e}"))
    })?;
    if status < 400 {
        normalize_chat_response(state, &mut json);
    }
    Ok((json, status))
}

pub fn normalize_chat_response(state: &LocalMlxProxyState, value: &mut Value) {
    strip_mlx_channels(value);

    let now = chrono::Utc::now().timestamp();
    if let Some(obj) = value.as_object_mut() {
        obj.entry("id".to_string())
            .or_insert_with(|| Value::String(format!("chatcmpl-gemma-{}", uuid::Uuid::new_v4())));
        obj.entry("object".to_string())
            .or_insert_with(|| Value::String("chat.completion".to_string()));
        obj.entry("created".to_string())
            .or_insert_with(|| Value::Number(now.into()));
        obj.entry("model".to_string())
            .or_insert_with(|| Value::String(state.alias.clone()));
        obj.entry("usage".to_string()).or_insert_with(|| {
            json!({
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "total_tokens": 0,
            })
        });

        if let Some(choices) = obj.get_mut("choices").and_then(Value::as_array_mut) {
            for (index, choice) in choices.iter_mut().enumerate() {
                normalize_choice(choice, index);
            }
        }
    }
}

fn normalize_choice(choice: &mut Value, index: usize) {
    let Some(obj) = choice.as_object_mut() else {
        return;
    };

    obj.entry("index".to_string())
        .or_insert_with(|| Value::Number((index as u64).into()));
    obj.entry("finish_reason".to_string())
        .or_insert(Value::Null);

    let message = obj
        .entry("message".to_string())
        .or_insert_with(|| json!({"role": "assistant", "content": ""}));

    if let Some(message_obj) = message.as_object_mut() {
        message_obj
            .entry("role".to_string())
            .or_insert_with(|| Value::String("assistant".to_string()));
        message_obj
            .entry("content".to_string())
            .or_insert_with(|| Value::String(String::new()));
        if message_obj.get("content") == Some(&Value::Null) {
            message_obj.insert("content".to_string(), Value::String(String::new()));
        }

        normalize_tool_calls(message_obj);
    }
}

fn normalize_tool_calls(message_obj: &mut serde_json::Map<String, Value>) {
    let Some(tool_calls) = message_obj
        .get_mut("tool_calls")
        .and_then(Value::as_array_mut)
    else {
        return;
    };

    for (index, tool_call) in tool_calls.iter_mut().enumerate() {
        let Some(obj) = tool_call.as_object_mut() else {
            continue;
        };

        obj.entry("id".to_string())
            .or_insert_with(|| Value::String(format!("call_{}", uuid::Uuid::new_v4().simple())));
        obj.entry("type".to_string())
            .or_insert_with(|| Value::String("function".to_string()));

        if !obj.contains_key("function") {
            if let Some(name) = obj.remove("name") {
                let arguments = obj
                    .remove("arguments")
                    .unwrap_or_else(|| Value::String("{}".to_string()));
                obj.insert(
                    "function".to_string(),
                    json!({
                        "name": name,
                        "arguments": normalize_arguments(arguments),
                    }),
                );
            }
        }

        if let Some(function) = obj.get_mut("function").and_then(Value::as_object_mut) {
            let arguments = function
                .remove("arguments")
                .unwrap_or_else(|| Value::String("{}".to_string()));
            function.insert("arguments".to_string(), normalize_arguments(arguments));
        }

        obj.entry("index".to_string())
            .or_insert_with(|| Value::Number((index as u64).into()));
    }
}

fn normalize_arguments(arguments: Value) -> Value {
    match arguments {
        Value::String(_) => arguments,
        other => Value::String(other.to_string()),
    }
}

pub fn models_json(state: &LocalMlxProxyState) -> Value {
    let mut ids = Vec::new();
    for id in [state.alias.as_str(), "gemma", "local-mlx"] {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    let data = ids
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "owned_by": "gemma",
                "root": state.model,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "object": "list",
        "data": data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> LocalMlxProxyState {
        LocalMlxProxyState {
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            alias: DEFAULT_ALIAS.to_string(),
            min_max_tokens: DEFAULT_MIN_MAX_TOKENS,
            total_requests: AtomicU64::new(0),
        }
    }

    #[test]
    fn normalize_chat_body_maps_gemma_alias_and_defaults_tokens() {
        let body = json!({
            "model": "gemma",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = normalize_chat_body(&state(), body);

        assert_eq!(out["model"], DEFAULT_MODEL);
        assert_eq!(out["max_tokens"], DEFAULT_MIN_MAX_TOKENS);
    }

    #[test]
    fn normalize_chat_body_preserves_max_tokens_above_local_mlx_minimum() {
        let body = json!({
            "model": "gemma",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 2048
        });
        let out = normalize_chat_body(&state(), body);

        assert_eq!(out["max_tokens"], 2048);
    }

    #[test]
    fn strip_mlx_channels_removes_reasoning_recursively() {
        let mut body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "ok",
                    "reasoning": "hidden"
                }
            }],
            "reasoning": "hidden"
        });

        strip_mlx_channels(&mut body);

        assert!(body.get("reasoning").is_none());
        assert!(body["choices"][0]["message"].get("reasoning").is_none());
        assert_eq!(body["choices"][0]["message"]["content"], "ok");
    }

    #[test]
    fn normalize_chat_response_synthesizes_required_openai_fields() {
        let mut body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning": "hidden"
                }
            }]
        });

        normalize_chat_response(&state(), &mut body);

        assert!(body["id"].as_str().unwrap().starts_with("chatcmpl-gemma-"));
        assert_eq!(body["object"], "chat.completion");
        assert!(body["created"].as_i64().is_some());
        assert_eq!(body["usage"]["total_tokens"], 0);
        assert_eq!(body["choices"][0]["message"]["content"], "");
        assert!(body["choices"][0]["message"].get("reasoning").is_none());
    }

    #[test]
    fn normalize_tool_calls_maps_flat_shape_to_openai_shape() {
        let mut body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "name": "lookup",
                        "arguments": {"q": "x"}
                    }]
                }
            }]
        });

        normalize_chat_response(&state(), &mut body);

        let tool_call = &body["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tool_call["type"], "function");
        assert_eq!(tool_call["function"]["name"], "lookup");
        assert_eq!(tool_call["function"]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn normalize_tool_calls_stringifies_nested_function_arguments() {
        let mut body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "lookup",
                            "arguments": {"q": "x"}
                        }
                    }]
                }
            }]
        });

        normalize_chat_response(&state(), &mut body);

        let tool_call = &body["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tool_call["function"]["arguments"], "{\"q\":\"x\"}");
    }

    #[test]
    fn normalize_chat_response_replaces_null_content_with_empty_string() {
        let mut body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null
                }
            }]
        });

        normalize_chat_response(&state(), &mut body);

        assert_eq!(body["choices"][0]["message"]["content"], "");
    }

    #[test]
    fn models_json_deduplicates_default_gemma_alias() {
        let models = models_json(&state());
        let data = models["data"].as_array().unwrap();

        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "gemma");
        assert_eq!(data[1]["id"], "local-mlx");
    }

    #[test]
    fn models_json_deduplicates_non_adjacent_aliases() {
        let mut state = state();
        state.alias = "local-mlx".to_string();

        let models = models_json(&state);
        let data = models["data"].as_array().unwrap();

        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "local-mlx");
        assert_eq!(data[1]["id"], "gemma");
    }
}
