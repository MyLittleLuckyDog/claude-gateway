pub mod session;
pub mod store;

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::codex::options::CodexOptions;
use crate::config::AppConfig;
use crate::error::GatewayError;

use self::session::{CodexAppSession, CodexAppSessionState, PendingApproval, MAX_CODEX_APP_HISTORY_SIZE};

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn codex_cli_path(options: &CodexOptions) -> String {
    options
        .cli_path
        .clone()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "codex".to_string())
}

fn next_rpc_id(session: &CodexAppSession) -> u64 {
    session.next_request_id.fetch_add(1, Ordering::Relaxed)
}

fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "id": id,
        "method": method,
        "params": params,
    })
}

fn initialize_request(id: u64) -> Value {
    rpc_request(
        id,
        "initialize",
        json!({
            "clientInfo": {
                "name": "claude-gateway",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": true,
            }
        }),
    )
}

fn thread_start_request(id: u64, options: &CodexOptions) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(cwd) = &options.cwd {
        params.insert("cwd".to_string(), Value::String(cwd.to_string_lossy().to_string()));
    }
    if let Some(model) = &options.model {
        params.insert("model".to_string(), Value::String(model.clone()));
        params.insert("modelProvider".to_string(), Value::String("openai".to_string()));
    }
    if let Some(system_prompt) = &options.system_prompt {
        params.insert("developerInstructions".to_string(), Value::String(system_prompt.clone()));
    }
    if let Some(approval_policy) = &options.approval_policy {
        params.insert("approvalPolicy".to_string(), Value::String(approval_policy.as_str().to_string()));
    }
    if let Some(sandbox) = &options.sandbox {
        params.insert("sandbox".to_string(), Value::String(sandbox.as_str().to_string()));
    }
    if options.ephemeral {
        params.insert("ephemeral".to_string(), Value::Bool(true));
    }
    rpc_request(id, "thread/start", Value::Object(params))
}

fn turn_start_request(id: u64, thread_id: &str, message: &str) -> Value {
    rpc_request(
        id,
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [
                {
                    "type": "text",
                    "text": message,
                }
            ]
        }),
    )
}

async fn send_json_line(tx: &mpsc::Sender<String>, value: &Value) -> Result<(), GatewayError> {
    let line = serde_json::to_string(value)
        .map_err(|e| GatewayError::Internal(format!("JSON encode error: {}", e)))?;
    tx.send(line)
        .await
        .map_err(|_| GatewayError::Internal("app-server stdin channel closed".to_string()))
}

async fn send_rpc_and_wait(
    session: &CodexAppSession,
    method: &str,
    params: Value,
) -> Result<Value, GatewayError> {
    let id = next_rpc_id(session);
    let (tx, rx) = oneshot::channel();
    session.pending_requests.lock().await.insert(id.to_string(), tx);
    let request = rpc_request(id, method, params);
    send_json_line(&session.stdin_tx, &request).await?;
    rx.await.map_err(|_| GatewayError::Internal(format!("app-server request `{}` dropped", method)))
}

fn build_history_event(event_type: &str, payload: Value) -> Arc<Value> {
    Arc::new(json!({
        "type": event_type,
        "payload": payload,
    }))
}

async fn record_event(session: &CodexAppSession, event: Arc<Value>) {
    let mut history = session.history.lock().await;
    history.push_back(event.clone());
    while history.len() > MAX_CODEX_APP_HISTORY_SIZE {
        history.pop_front();
    }
    drop(history);
    let _ = session.event_tx.send(event);
}

async fn handle_incoming_value(session: Arc<CodexAppSession>, value: Value) {
    if let Some(id) = value.get("id").and_then(|v| {
        v.as_u64()
            .map(|n| n.to_string())
            .or_else(|| v.as_str().map(|s| s.to_string()))
    }) {
        if value.get("result").is_some() && value.get("method").is_none() {
            if let Some(tx) = session.pending_requests.lock().await.remove(&id) {
                let _ = tx.send(value["result"].clone());
                return;
            }
        }
    }

    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "thread/started" => {
                if let Some(thread_id) = params
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                {
                    *session.thread_id.lock().await = Some(thread_id.to_string());
                }
                record_event(session.as_ref(), build_history_event("thread_started", params)).await;
            }
            "turn/started" => {
                if let Some(turn_id) = params
                    .get("turn")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                {
                    *session.turn_id.lock().await = Some(turn_id.to_string());
                }
                *session.state.lock().await = CodexAppSessionState::Running;
                record_event(session.as_ref(), build_history_event("turn_started", params)).await;
            }
            "turn/completed" => {
                *session.turn_id.lock().await = None;
                *session.pending_approval.lock().await = None;
                *session.state.lock().await = CodexAppSessionState::Idle;
                record_event(session.as_ref(), build_history_event("turn_completed", params)).await;
            }
            "thread/status/changed" => {
                record_event(session.as_ref(), build_history_event("thread_status_changed", params)).await;
            }
            "item/agentMessage/delta" => {
                record_event(session.as_ref(), build_history_event("agent_message_delta", params)).await;
            }
            "item/started" => {
                record_event(session.as_ref(), build_history_event("item_started", params)).await;
            }
            "item/completed" => {
                record_event(session.as_ref(), build_history_event("item_completed", params)).await;
            }
            "thread/tokenUsage/updated" => {
                record_event(session.as_ref(), build_history_event("token_usage_updated", params)).await;
            }
            "serverRequest/resolved" => {
                record_event(session.as_ref(), build_history_event("server_request_resolved", params)).await;
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval" => {
                let request_id = value
                    .get("id")
                    .and_then(|v| {
                        v.as_u64()
                            .map(|n| n.to_string())
                            .or_else(|| v.as_str().map(|s| s.to_string()))
                    })
                    .unwrap_or_default();
                *session.pending_approval.lock().await = Some(PendingApproval {
                    request_id: request_id.clone(),
                    method: method.to_string(),
                });
                *session.state.lock().await = CodexAppSessionState::WaitingForApproval {
                    request_id: request_id.clone(),
                    method: method.to_string(),
                };
                record_event(
                    session.as_ref(),
                    Arc::new(json!({
                        "type": "approval_request",
                        "request_id": request_id,
                        "method": method,
                        "params": params,
                    })),
                )
                .await;
            }
            _ => {
                record_event(
                    session.as_ref(),
                    Arc::new(json!({
                        "type": "rpc_notification",
                        "method": method,
                        "params": params,
                    })),
                )
                .await;
            }
        }
    }
}

async fn stdout_parser_task(
    stdout: tokio::process::ChildStdout,
    session: Arc<CodexAppSession>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => handle_incoming_value(session.clone(), value).await,
            Err(e) => {
                let event = build_history_event(
                    "error",
                    json!({
                        "message": format!("JSON decode error: {}", e),
                        "code": "json_decode",
                    }),
                );
                record_event(session.as_ref(), event).await;
            }
        }
    }
    *session.state.lock().await = CodexAppSessionState::Dead;
}

async fn stdin_writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(data) = rx.recv().await {
        let line = if data.ends_with('\n') {
            data
        } else {
            format!("{}\n", data)
        };
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn stderr_logger_task(stderr: tokio::process::ChildStderr) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tracing::debug!("codex app-server stderr: {}", line);
        }
    }
}

pub async fn create_session(
    options: CodexOptions,
    store: Arc<store::CodexAppSessionStore>,
    _config: Arc<AppConfig>,
) -> Result<Arc<CodexAppSession>, GatewayError> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(codex_cli_path(&options));
    cmd.arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = &options.env {
        for (key, value) in env {
            cmd.env(key, value);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| GatewayError::CliConnection(format!("Failed to spawn Codex app-server: {}", e)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GatewayError::CliConnection("No stdout pipe from Codex app-server".to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| GatewayError::CliConnection("No stdin pipe from Codex app-server".to_string()))?;
    let stderr = child.stderr.take();

    let (stdin_tx, stdin_rx) = mpsc::channel::<String>(64);
    let (event_tx, _) = broadcast::channel::<Arc<Value>>(1024);
    let session = Arc::new(CodexAppSession {
        id: session_id.clone(),
        thread_id: Arc::new(Mutex::new(None)),
        turn_id: Arc::new(Mutex::new(None)),
        state: Arc::new(Mutex::new(CodexAppSessionState::Initializing)),
        created_at: std::time::Instant::now(),
        last_activity_ms: AtomicU64::new(now_epoch_ms()),
        options,
        stdin_tx,
        event_tx,
        history: Arc::new(Mutex::new(VecDeque::new())),
        pending_requests: Arc::new(Mutex::new(HashMap::new())),
        pending_approval: Arc::new(Mutex::new(None)),
        next_request_id: AtomicU64::new(1000),
    });

    store.insert(session.clone())?;

    tokio::spawn(stdout_parser_task(stdout, session.clone()));
    tokio::spawn(stdin_writer_task(stdin, stdin_rx));
    if let Some(stderr) = stderr {
        tokio::spawn(stderr_logger_task(stderr));
    }
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    let init_id = next_rpc_id(session.as_ref());
    let (tx, rx) = oneshot::channel();
    session.pending_requests.lock().await.insert(init_id.to_string(), tx);
    send_json_line(&session.stdin_tx, &initialize_request(init_id)).await?;
    rx.await.map_err(|_| GatewayError::Internal("app-server initialize dropped".to_string()))?;

    let thread_result = send_rpc_and_wait(
        session.as_ref(),
        "thread/start",
        thread_start_request(next_rpc_id(session.as_ref()), &session.options)["params"].clone(),
    )
    .await?;

    if let Some(thread_id) = thread_result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
    {
        *session.thread_id.lock().await = Some(thread_id.to_string());
    }
    *session.state.lock().await = CodexAppSessionState::Idle;

    Ok(session)
}

pub async fn send_turn(session: Arc<CodexAppSession>, message: String) -> Result<(), GatewayError> {
    let thread_id = session
        .thread_id
        .lock()
        .await
        .clone()
        .ok_or_else(|| GatewayError::InvalidSessionState {
            expected: "initialized thread".to_string(),
            actual: "missing thread_id".to_string(),
        })?;
    let request_id = next_rpc_id(session.as_ref());
    let (tx, rx) = oneshot::channel();
    session
        .pending_requests
        .lock()
        .await
        .insert(request_id.to_string(), tx);
    *session.state.lock().await = CodexAppSessionState::Running;
    session.last_activity_ms.store(now_epoch_ms(), Ordering::Relaxed);
    send_json_line(&session.stdin_tx, &turn_start_request(request_id, &thread_id, &message)).await?;
    let _ = rx
        .await
        .map_err(|_| GatewayError::Internal("app-server turn/start dropped".to_string()))?;
    Ok(())
}

pub async fn send_approval_response(
    session: Arc<CodexAppSession>,
    request_id: &str,
    response: Value,
) -> Result<(), GatewayError> {
    let current = session.pending_approval.lock().await.clone();
    let Some(pending) = current else {
        return Err(GatewayError::InvalidSessionState {
            expected: "waiting_for_approval".to_string(),
            actual: session.state.lock().await.to_string(),
        });
    };
    if pending.request_id != request_id {
        return Err(GatewayError::InvalidSessionState {
            expected: format!("waiting for approval {}", request_id),
            actual: format!("waiting for approval {}", pending.request_id),
        });
    }
    let id_num = request_id
        .parse::<u64>()
        .map_err(|_| GatewayError::Internal(format!("invalid approval request id: {}", request_id)))?;
    let msg = json!({
        "id": id_num,
        "result": response,
    });
    send_json_line(&session.stdin_tx, &msg).await?;
    *session.state.lock().await = CodexAppSessionState::Running;
    session.last_activity_ms.store(now_epoch_ms(), Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rpc_request;
    use serde_json::json;

    #[test]
    fn builds_rpc_request() {
        let value = rpc_request(7, "turn/start", json!({"threadId":"t"}));
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "turn/start");
        assert_eq!(value["params"]["threadId"], "t");
    }
}
