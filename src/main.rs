//! claude-gateway — Claude Code CLI를 래핑하는 REST API 게이트웨이.
//!
//! Claude Code CLI(`claude`)를 subprocess로 호출하여 LLM 응답을 HTTP API로 제공.
//! Claude Code 토큰 풀(구독 과금) 사용. Python 불필요, 단일 바이너리.
//!
//! 사용법:
//!   claude-gateway                           # 기본 포트 8100
//!   claude-gateway --port 8200               # 포트 변경
//!   claude-gateway --model claude-sonnet-4-6 # 모델 변경
//!
//! API:
//!   POST /chat   { prompt, system_prompt?, model? } → { response, error? }
//!   POST /reset  → { status }
//!   GET  /health → { status, model, claude_cli }

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AppConfig {
    model: String,
    port: u16,
}

impl AppConfig {
    fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut model = "claude-haiku-4-5".to_string();
        let mut port = 8100u16;

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--model" | "-m" => {
                    if i + 1 < args.len() {
                        model = args[i + 1].clone();
                        i += 1;
                    }
                }
                "--port" | "-p" => {
                    if i + 1 < args.len() {
                        port = args[i + 1].parse().unwrap_or(8100);
                        i += 1;
                    }
                }
                "--help" | "-h" => {
                    eprintln!("Usage: claude-gateway [--port PORT] [--model MODEL]");
                    eprintln!("  --port PORT     Listen port (default: 8100)");
                    eprintln!("  --model MODEL   Claude model (default: claude-haiku-4-5)");
                    std::process::exit(0);
                }
                _ => {}
            }
            i += 1;
        }

        Self { model, port }
    }
}

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatRequest {
    prompt: String,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Serialize)]
struct ChatResponse {
    response: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    model: String,
    claude_cli: bool,
}

// ---------------------------------------------------------------------------
// Claude CLI 호출
// ---------------------------------------------------------------------------

fn call_claude_cli(
    prompt: &str,
    system_prompt: Option<&str>,
    model: &str,
) -> Result<String, String> {
    let claude_path = which_claude().ok_or_else(|| {
        "claude CLI not found. Install: npm install -g @anthropic-ai/claude-code".to_string()
    })?;

    let mut args = vec![
        "-p".to_string(),
        prompt.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--model".to_string(),
        model.to_string(),
    ];

    if let Some(sp) = system_prompt {
        args.push("--system-prompt".to_string());
        args.push(sp.to_string());
    }

    let child = Command::new(&claude_path)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {e}"))?;

    let stdout = child.stdout.ok_or("No stdout")?;
    let reader = BufReader::new(stdout);

    let mut full_response = String::new();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("Read error: {e}"))?;
        if line.is_empty() {
            continue;
        }

        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
            // assistant 메시지에서 텍스트 추출
            if msg.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(content) = msg
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                full_response.push_str(text);
                            }
                        }
                    }
                }
            }
            // result 메시지
            if msg.get("type").and_then(|t| t.as_str()) == Some("result") {
                if let Some(result) = msg.get("result").and_then(|r| r.as_str()) {
                    if full_response.is_empty() {
                        full_response = result.to_string();
                    }
                }
            }
        }
    }

    if full_response.is_empty() {
        Err("Empty response from claude CLI".to_string())
    } else {
        Ok(full_response)
    }
}

fn which_claude() -> Option<String> {
    if let Ok(output) = Command::new("which").arg("claude").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    for path in &["/usr/local/bin/claude", "/opt/homebrew/bin/claude"] {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    None
}

fn check_claude_available() -> bool {
    which_claude().is_some()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

type AppState = Arc<Mutex<AppConfig>>;

async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let config = state.lock().await;
    Json(HealthResponse {
        status: "ok".to_string(),
        model: config.model.clone(),
        claude_cli: check_claude_available(),
    })
}

async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ChatResponse>)> {
    let config = state.lock().await;
    let model = req.model.as_deref().unwrap_or(&config.model).to_string();
    drop(config);

    tracing::info!(prompt_len = req.prompt.len(), model = %model, "chat request");

    let prompt = req.prompt.clone();
    let system_prompt = req.system_prompt.clone();

    // blocking CLI 호출을 spawn_blocking으로 실행 — axum 이벤트 루프 블로킹 방지
    let result = tokio::task::spawn_blocking(move || {
        call_claude_cli(&prompt, system_prompt.as_deref(), &model)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ChatResponse {
                response: String::new(),
                error: Some(format!("Task error: {e}")),
            }),
        )
    })?;

    match result {
        Ok(response) => {
            tracing::info!(response_len = response.len(), "chat response");
            Ok(Json(ChatResponse {
                response,
                error: None,
            }))
        }
        Err(e) => {
            tracing::error!(error = %e, "chat error");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ChatResponse {
                    response: String::new(),
                    error: Some(e),
                }),
            ))
        }
    }
}

async fn reset_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "ok", "message": "session reset"}))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = AppConfig::from_args();
    let port = config.port;

    if !check_claude_available() {
        eprintln!("⚠ claude CLI not found in PATH");
        eprintln!("  Install: npm install -g @anthropic-ai/claude-code");
    }

    tracing::info!(port = port, model = %config.model, "Starting claude-gateway");

    let state: AppState = Arc::new(Mutex::new(config));

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/chat", post(chat_handler))
        .route("/reset", post(reset_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("Failed to bind port");

    println!("claude-gateway listening on http://0.0.0.0:{port}");
    axum::serve(listener, app).await.unwrap();
}
