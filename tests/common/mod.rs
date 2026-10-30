//! Shared harness for the proxy integration tests: a hermetic credentials
//! environment and a local stub standing in for api.anthropic.com that records
//! every request body the proxy sends and replays scripted responses.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use claude_agent::api::{build_router, AppState, Stats};
use claude_agent::codex::store::CodexSessionStore;
use claude_agent::codex_app::store::CodexAppSessionStore;
use claude_agent::config::AppConfig;
use claude_agent::proxy::ProxyState;
use claude_agent::proxy_session::ProxySessionStore;
use claude_agent::session::store::SessionStore;
use serde_json::{json, Value};

// ── Hermetic environment ───────────────────────────────────────────

/// Credentials and identity live in process-global env/cache, so set them up
/// once for the whole test binary: a throwaway HOME with a fake OAuth token,
/// and the keychain off. Nothing here reads the developer's real credentials.
pub fn init_env() {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join(".claude");
        std::fs::create_dir_all(&config).expect("create config dir");
        std::fs::write(
            config.join(".credentials.json"),
            json!({"claudeAiOauth": {
                "accessToken": "test-access-token",
                "refreshToken": "test-refresh-token",
                "expiresAt": 4_102_444_800_000i64, // 2100-01-01
            }})
            .to_string(),
        )
        .expect("write credentials");
        // Runs inside get_or_init, before any test touches auth or
        // client_identity.
        std::env::set_var("HOME", dir.path());
        std::env::set_var("CLAUDE_CONFIG_DIR", &config);
        std::env::set_var("CLAUDE_GATEWAY_SKIP_KEYCHAIN", "1");
        dir
    });
}

// ── Upstream stub ──────────────────────────────────────────────────

/// One scripted upstream response.
pub struct Scripted {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Value,
}

impl From<Value> for Scripted {
    /// A bare body is a plain 200.
    fn from(body: Value) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
        }
    }
}

#[derive(Default)]
pub struct Upstream {
    /// Every request body received, in order.
    pub requests: Mutex<Vec<Value>>,
    /// The headers of each request, in the same order as `requests`.
    pub request_headers: Mutex<Vec<HeaderMap>>,
    /// Path and query of every `GET` the stub served.
    pub get_requests: Mutex<Vec<String>>,
    /// When set, `count_tokens` counts the way a server would -- one token per
    /// text character, 1,000 per image, 7 of fixed overhead per message --
    /// instead of answering a flat 42. Stands in for upstream's tokenizer in
    /// tests that need counts to follow content.
    pub count_by_content: std::sync::atomic::AtomicBool,
    /// When set, `count_tokens` answers 400.
    pub count_fails: std::sync::atomic::AtomicBool,
    /// Scripted responses, consumed front to back; empty → a normal reply.
    pub script: Mutex<VecDeque<Scripted>>,
}

impl Upstream {
    pub fn push(&self, scripted: impl Into<Scripted>) {
        self.script.lock().expect("lock").push_back(scripted.into());
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("lock").len()
    }

    /// Value of `name` on the most recent upstream request, if any.
    pub fn last_header(&self, name: &str) -> Option<String> {
        self.request_headers
            .lock()
            .expect("lock")
            .last()
            .and_then(|h| h.get(name))
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }
}

pub fn ok_reply(model: &str, input_tokens: u64) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input_tokens, "output_tokens": 2},
    })
}

pub fn refusal_reply(model: &str) -> Value {
    json!({
        "id": "msg_refused",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"category": "cyber"},
        "usage": {"input_tokens": 1000, "output_tokens": 0},
    })
}

/// A minimal streamed reply, shaped like the real Messages SSE feed.
fn sse_reply(model: &str) -> Response {
    let events = [
        json!({"type": "message_start", "message": {
            "id": "msg_stream", "type": "message", "role": "assistant", "model": model,
            "content": [], "usage": {"input_tokens": 10, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ];
    let body: String = events
        .iter()
        .map(|e| {
            format!(
                "event: {}\ndata: {}\n\n",
                e["type"].as_str().unwrap_or(""),
                e
            )
        })
        .collect();
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    (StatusCode::OK, headers, body).into_response()
}

async fn upstream_handler(
    State(up): State<Arc<Upstream>>,
    request_headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let wants_stream = body.get("stream") == Some(&Value::Bool(true));
    up.requests.lock().expect("lock").push(body);
    up.request_headers
        .lock()
        .expect("lock")
        .push(request_headers);
    let scripted = up.script.lock().expect("lock").pop_front();
    if scripted.is_none() && wants_stream {
        return sse_reply(&model);
    }
    let scripted = scripted.unwrap_or_else(|| ok_reply(&model, 10).into());

    let mut headers = HeaderMap::new();
    for (name, value) in scripted.headers {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(&value).expect("header value"),
        );
    }
    let status = StatusCode::from_u16(scripted.status).expect("status");
    // A string body is sent as it is, as text -- what an edge proxy or load
    // balancer in front of the API sends instead of a JSON error.
    if let Value::String(text) = scripted.body {
        headers.insert("content-type", HeaderValue::from_static("text/html"));
        return (status, headers, text).into_response();
    }
    (status, headers, Json(scripted.body)).into_response()
}

/// The stub's tokenizer for `count_by_content`: see [`Upstream::count_by_content`].
fn content_tokens(request: &Value) -> u64 {
    let block_tokens = |block: &Value| match block.get("type").and_then(Value::as_str) {
        Some("text") => block["text"]
            .as_str()
            .map_or(0, |t| t.chars().count() as u64),
        Some("image") => 1_000,
        _ => 10,
    };
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            7 + match &m["content"] {
                Value::String(s) => s.chars().count() as u64,
                Value::Array(blocks) => blocks.iter().map(block_tokens).sum(),
                _ => 0,
            }
        })
        .sum()
}

/// `count_tokens`: records the request like `/v1/messages` does and answers
/// with a fixed count, or a content-based one (see [`Upstream::count_by_content`]).
async fn count_tokens_handler(
    State(up): State<Arc<Upstream>>,
    request_headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    use std::sync::atomic::Ordering::Relaxed;
    let tokens = if up.count_by_content.load(Relaxed) {
        content_tokens(&body)
    } else {
        42
    };
    up.requests.lock().expect("lock").push(body);
    up.request_headers
        .lock()
        .expect("lock")
        .push(request_headers);
    if up.count_fails.load(Relaxed) {
        let err = json!({"type": "error", "error": {"type": "invalid_request_error",
                                                    "message": "cannot count this"}});
        return (StatusCode::BAD_REQUEST, Json(err)).into_response();
    }
    Json(json!({"input_tokens": tokens})).into_response()
}

fn model_json(id: &str) -> Value {
    json!({"type": "model", "id": id, "display_name": format!("Model {id}"),
           "created_at": "2026-01-01T00:00:00Z",
           "max_input_tokens": 1_000_000, "max_tokens": 128_000})
}

async fn models_list_handler(
    State(up): State<Arc<Upstream>>,
    RawQuery(query): RawQuery,
) -> Json<Value> {
    let path = match query {
        Some(q) => format!("/v1/models?{q}"),
        None => "/v1/models".to_string(),
    };
    up.get_requests.lock().expect("lock").push(path);
    Json(
        json!({"data": [model_json("claude-sonnet-5-5")], "has_more": false,
                "first_id": "claude-sonnet-5-5", "last_id": "claude-sonnet-5-5"}),
    )
}

async fn model_get_handler(State(up): State<Arc<Upstream>>, Path(id): Path<String>) -> Json<Value> {
    up.get_requests
        .lock()
        .expect("lock")
        .push(format!("/v1/models/{id}"));
    Json(model_json(&id))
}

/// Start just the upstream stub on an ephemeral port; returns its base URL.
pub async fn start_stub() -> (String, Arc<Upstream>) {
    init_env();
    let up = Arc::new(Upstream::default());
    let app = Router::new()
        .route("/v1/messages", post(upstream_handler))
        .route("/v1/messages/count_tokens", post(count_tokens_handler))
        .route("/v1/models", get(models_list_handler))
        .route("/v1/models/:id", get(model_get_handler))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), up)
}

/// Start the stub on an ephemeral port and a proxy pointed at it.
pub async fn start() -> (Arc<ProxyState>, Arc<Upstream>) {
    let (base, up) = start_stub().await;
    let state = Arc::new(ProxyState::with_base_url(4, base));
    (state, up)
}

pub fn user_turns(n: usize) -> Vec<Value> {
    // Alternating user/assistant history ending on a user turn.
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                json!({"role": "user", "content": format!("question {i}")})
            } else {
                json!({"role": "assistant", "content": format!("answer {i}")})
            }
        })
        .collect()
}

/// Like [`start`], but also serves the real gateway router (every route, CORS
/// layer and error mapping included) in front of the stub and returns its
/// base URL, so tests can speak HTTP to the gateway the way a client would.
pub async fn start_with_gateway() -> (String, Arc<ProxyState>, Arc<Upstream>) {
    let (proxy, up) = start().await;
    let base = serve_gateway(Some(proxy.clone())).await;
    (base, proxy, up)
}

/// The real router with the direct API proxy switched off, as when no OAuth
/// credentials are available.
pub async fn start_gateway_without_proxy() -> String {
    init_env();
    serve_gateway(None).await
}

pub async fn serve_gateway(proxy: Option<Arc<ProxyState>>) -> String {
    let config = Arc::new(AppConfig::default());
    let state = AppState {
        config,
        sessions: Arc::new(SessionStore::new(4)),
        codex_sessions: Arc::new(CodexSessionStore::new(4)),
        codex_app_sessions: Arc::new(CodexAppSessionStore::new(4)),
        start_time: std::time::Instant::now(),
        stats: Arc::new(tokio::sync::Mutex::new(Stats::default())),
        proxy_sessions: proxy
            .is_some()
            .then(|| Arc::new(ProxySessionStore::new(8, 3600))),
        proxy_unavailable: proxy
            .is_none()
            .then(|| "the test started the gateway without credentials".to_string()),
        proxy,
        local_mlx: None,
        openai: None,
        openai_oauth: None,
    };
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let addr = listener.local_addr().expect("gateway addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}
