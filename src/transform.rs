use serde_json::{json, Value};

/// Returns true if the model name should be routed to OpenAI ChatCompletion.
pub fn is_openai_model(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("gpt-") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

/// Convert an Anthropic Messages request body to OpenAI ChatCompletion format.
pub fn anthropic_to_openai_chat(body: &Value) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // system → first system message
    if let Some(system) = body.get("system") {
        let text = extract_system_text(system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    // messages array
    if let Some(arr) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in arr {
            messages.push(convert_message(msg));
        }
    }

    let mut out = json!({
        "model": body.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
    });

    // max_tokens passthrough
    if let Some(mt) = body.get("max_tokens") {
        out["max_tokens"] = mt.clone();
    }

    // temperature, top_p passthrough
    for key in &["temperature", "top_p", "stream"] {
        if let Some(v) = body.get(*key) {
            out[*key] = v.clone();
        }
    }

    out
}

/// Convert an OpenAI ChatCompletion response body to Anthropic Messages format.
pub fn openai_chat_to_anthropic(body: &Value) -> Value {
    let text = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let finish_reason = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("stop");

    let stop_reason = match finish_reason {
        "length" => "max_tokens",
        _ => "end_turn",
    };

    let input_tokens = body
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let output_tokens = body
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let model = body.get("model").cloned().unwrap_or(Value::Null);

    json!({
        "content": [{"type": "text", "text": text}],
        "model": model,
        "stop_reason": stop_reason,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    })
}

// ── Helpers ────────────────────────────────────────────────────────

fn extract_system_text(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn convert_message(msg: &Value) -> Value {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let content = msg.get("content");

    match content {
        // String content — passthrough
        Some(Value::String(s)) => json!({"role": role, "content": s}),

        // Array content — convert each block
        Some(Value::Array(blocks)) => {
            let converted: Vec<Value> = blocks.iter().map(convert_content_block).collect();
            json!({"role": role, "content": converted})
        }

        // null / missing — empty string
        _ => json!({"role": role, "content": ""}),
    }
}

fn convert_content_block(block: &Value) -> Value {
    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("text");

    match block_type {
        "image" => {
            if let Some(source) = block.get("source") {
                let media_type = source
                    .get("media_type")
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png");
                let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                let url = format!("data:{media_type};base64,{data}");
                return json!({"type": "image_url", "image_url": {"url": url}});
            }
            // Unexpected image block shape — return text placeholder
            json!({"type": "text", "text": "[image]"})
        }

        "text" => {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            json!({"type": "text", "text": text})
        }

        // Unknown block types — pass text if available, else empty
        _ => {
            let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
            json!({"type": "text", "text": text})
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_openai_model_detects_gpt() {
        assert!(is_openai_model("gpt-4o"));
        assert!(is_openai_model("gpt-3.5-turbo"));
    }

    #[test]
    fn is_openai_model_detects_o_series() {
        assert!(is_openai_model("o1-mini"));
        assert!(is_openai_model("o3-2025-04-16"));
        assert!(is_openai_model("o4-mini"));
    }

    #[test]
    fn is_openai_model_excludes_claude() {
        assert!(!is_openai_model("claude-sonnet-4-6"));
        assert!(!is_openai_model("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn convert_string_system() {
        let body = json!({
            "model": "gpt-4o",
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 100
        });
        let out = anthropic_to_openai_chat(&body);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "You are a helpful assistant.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(out["max_tokens"], 100);
    }

    #[test]
    fn convert_array_system_with_cache_control() {
        let body = json!({
            "model": "gpt-4o",
            "system": [
                {"type": "text", "text": "You are an agent.", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [{"role": "user", "content": "Do something."}],
            "max_tokens": 1000
        });
        let out = anthropic_to_openai_chat(&body);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], "You are an agent.");
        assert!(out.get("cache_control").is_none());
    }

    #[test]
    fn convert_image_block() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is this?"},
                    {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "abc123"
                        }
                    }
                ]
            }]
        });
        let out = anthropic_to_openai_chat(&body);
        let content = &out["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/png;base64,abc123"
        );
    }

    #[test]
    fn response_conversion() {
        let openai_resp = json!({
            "choices": [{"message": {"content": "Hello!"}, "finish_reason": "stop"}],
            "model": "gpt-4o",
            "usage": {"prompt_tokens": 100, "completion_tokens": 50}
        });
        let out = openai_chat_to_anthropic(&openai_resp);
        assert_eq!(out["content"][0]["text"], "Hello!");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 100);
        assert_eq!(out["usage"]["output_tokens"], 50);
    }

    #[test]
    fn response_length_finish_reason() {
        let openai_resp = json!({
            "choices": [{"message": {"content": "..."}, "finish_reason": "length"}],
            "model": "gpt-4o",
            "usage": {"prompt_tokens": 10, "completion_tokens": 1000}
        });
        let out = openai_chat_to_anthropic(&openai_resp);
        assert_eq!(out["stop_reason"], "max_tokens");
    }
}
