//! Shared harness for the proxy integration tests: a hermetic credentials
//! environment and a local stub standing in for api.anthropic.com that records
//! every request body the proxy sends and replays scripted responses.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use claude_agent::proxy::ProxyState;
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

async fn upstream_handler(State(up): State<Arc<Upstream>>, Json(body): Json<Value>) -> Response {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    up.requests.lock().expect("lock").push(body);
    let scripted = up
        .script
        .lock()
        .expect("lock")
        .pop_front()
        .unwrap_or_else(|| ok_reply(&model, 10).into());

    let mut headers = HeaderMap::new();
    for (name, value) in scripted.headers {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(&value).expect("header value"),
        );
    }
    let status = StatusCode::from_u16(scripted.status).expect("status");
    (status, headers, Json(scripted.body)).into_response()
}

/// Start the stub on an ephemeral port and a proxy pointed at it.
pub async fn start() -> (Arc<ProxyState>, Arc<Upstream>) {
    init_env();
    let up = Arc::new(Upstream::default());
    let app = Router::new()
        .route("/v1/messages", post(upstream_handler))
        .with_state(up.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let addr = listener.local_addr().expect("stub addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let state = Arc::new(ProxyState::with_base_url(4, format!("http://{addr}")));
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
