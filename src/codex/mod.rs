pub mod messages;
pub mod options;
pub mod session;
pub mod store;

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use crate::config::AppConfig;
use crate::error::GatewayError;

use self::messages::{
    CodexCommandItem, CodexEvent, CodexItem, CodexMessageItem, CodexQueryResult, CodexTurnUsage,
    RawCodexEvent,
};
use self::options::{CodexApprovalPolicy, CodexOptions, CodexSandboxMode};
use self::session::{CodexSession, CodexSessionState, MAX_CODEX_HISTORY_SIZE};

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

fn default_approval_policy(options: &CodexOptions) -> CodexApprovalPolicy {
    options
        .approval_policy
        .clone()
        .unwrap_or(CodexApprovalPolicy::Never)
}

fn effective_prompt(prompt: &str, options: &CodexOptions, has_existing_thread: bool) -> String {
    match (&options.system_prompt, has_existing_thread) {
        (Some(system_prompt), false) if !system_prompt.trim().is_empty() => {
            format!(
                "<system>\n{}\n</system>\n\n<user>\n{}\n</user>",
                system_prompt, prompt
            )
        }
        _ => prompt.to_string(),
    }
}

fn configure_base_command(
    cmd: &mut Command,
    options: &CodexOptions,
) {
    if let Some(model) = &options.model {
        cmd.arg("--model").arg(model);
    }

    if let Some(profile) = &options.profile {
        cmd.arg("--profile").arg(profile);
    }

    let approval = default_approval_policy(options);
    cmd.arg("--ask-for-approval").arg(approval.as_str());

    let sandbox = options
        .sandbox
        .clone()
        .unwrap_or(CodexSandboxMode::ReadOnly);
    cmd.arg("--sandbox").arg(sandbox.as_str());

    if options.full_auto {
        cmd.arg("--full-auto");
    }
    if options.dangerously_bypass_approvals_and_sandbox {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    }
    if options.search {
        cmd.arg("--search");
    }
    if options.ephemeral {
        cmd.arg("--ephemeral");
    }
    if options.ignore_user_config {
        cmd.arg("--ignore-user-config");
    }
    if options.ignore_rules {
        cmd.arg("--ignore-rules");
    }
    if options.skip_git_repo_check {
        cmd.arg("--skip-git-repo-check");
    }
    if let Some(cwd) = &options.cwd {
        cmd.arg("--cd").arg(cwd);
    }
    if let Some(add_dirs) = &options.add_dirs {
        for dir in add_dirs {
            cmd.arg("--add-dir").arg(dir);
        }
    }
    if let Some(env) = &options.env {
        for (key, value) in env {
            cmd.env(key, value);
        }
    }
}

fn build_exec_command(
    prompt: &str,
    options: &CodexOptions,
    thread_id: Option<&str>,
) -> Command {
    let mut cmd = Command::new(codex_cli_path(options));
    configure_base_command(&mut cmd, options);
    cmd.arg("exec");
    if let Some(thread_id) = thread_id {
        cmd.arg("resume").arg(thread_id);
    }
    cmd.arg("--json");
    cmd.arg(prompt);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

fn raw_to_event(raw: RawCodexEvent) -> Option<CodexEvent> {
    match raw {
        RawCodexEvent::ThreadStarted { thread_id } => Some(CodexEvent::ThreadStarted { thread_id }),
        RawCodexEvent::TurnStarted => Some(CodexEvent::TurnStarted),
        RawCodexEvent::TurnCompleted { usage } => Some(CodexEvent::TurnCompleted { usage }),
        RawCodexEvent::ItemStarted { item } => match item {
            CodexItem::CommandExecution(item) => Some(CodexEvent::CommandExecution {
                item: CodexCommandItem::from(item),
                completed: false,
            }),
            CodexItem::AgentMessage(item) => Some(CodexEvent::AgentMessage {
                item: CodexMessageItem::from(item),
                completed: false,
            }),
        },
        RawCodexEvent::ItemCompleted { item } => match item {
            CodexItem::CommandExecution(item) => Some(CodexEvent::CommandExecution {
                item: CodexCommandItem::from(item),
                completed: true,
            }),
            CodexItem::AgentMessage(item) => Some(CodexEvent::AgentMessage {
                item: CodexMessageItem::from(item),
                completed: true,
            }),
        },
    }
}

async fn run_command_collect(
    prompt: &str,
    options: &CodexOptions,
    thread_id: Option<&str>,
) -> Result<CodexQueryResult, GatewayError> {
    let prompt = effective_prompt(prompt, options, thread_id.is_some());
    let mut cmd = build_exec_command(&prompt, options, thread_id);
    tracing::debug!("spawning Codex CLI: {:?}", cmd.as_std());

    let mut child = cmd.spawn().map_err(|e| {
        GatewayError::CliConnection(format!("Failed to spawn Codex CLI: {}", e))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| GatewayError::CliConnection("No stdout pipe from Codex".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| GatewayError::CliConnection("No stderr pipe from Codex".to_string()))?;

    let stdout_task = tokio::spawn(async move {
        let mut events = Vec::new();
        let mut thread_id = None;
        let mut last_message = None;
        let mut usage: Option<CodexTurnUsage> = None;

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.starts_with('{') {
                continue;
            }
            let raw: RawCodexEvent = serde_json::from_str(trimmed).map_err(|e| {
                GatewayError::JsonDecode {
                    line: trimmed.to_string(),
                    source: e,
                }
            })?;
            if let RawCodexEvent::ThreadStarted { thread_id: ref tid } = raw {
                thread_id = Some(tid.clone());
            }
            if let RawCodexEvent::TurnCompleted { usage: ref turn_usage } = raw {
                usage = Some(turn_usage.clone());
            }
            if let RawCodexEvent::ItemCompleted {
                item: CodexItem::AgentMessage(ref item),
            } = raw
            {
                last_message = Some(item.text.clone());
            }
            if let Some(event) = raw_to_event(raw) {
                events.push(event);
            }
        }

        Ok::<_, GatewayError>((events, thread_id, last_message, usage))
    });

    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut stderr_lines = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if !line.trim().is_empty() {
                stderr_lines.push(line);
            }
        }
        Ok::<_, std::io::Error>(stderr_lines.join("\n"))
    });

    let status = child.wait().await?;
    let (events, observed_thread_id, last_message, usage) = stdout_task
        .await
        .map_err(|e| GatewayError::Internal(format!("Codex stdout task join error: {}", e)))??;
    let stderr = stderr_task
        .await
        .map_err(|e| GatewayError::Internal(format!("Codex stderr task join error: {}", e)))??;

    if !status.success() {
        return Err(match status.code() {
            Some(code) => GatewayError::ProcessExit {
                exit_code: code,
                stderr,
            },
            None => GatewayError::ProcessCrash {
                detail: stderr,
            },
        });
    }

    Ok(CodexQueryResult {
        thread_id: observed_thread_id,
        output_text: last_message,
        usage,
        events,
    })
}

pub async fn query(
    prompt: &str,
    options: CodexOptions,
    _config: &AppConfig,
) -> Result<CodexQueryResult, GatewayError> {
    run_command_collect(prompt, &options, None).await
}

pub async fn query_stream(
    prompt: &str,
    options: CodexOptions,
    _config: &AppConfig,
) -> Result<tokio::sync::mpsc::Receiver<CodexEvent>, GatewayError> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        let prompt = effective_prompt(&prompt, &options, false);
        let mut cmd = build_exec_command(&prompt, &options, None);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = tx
                    .send(CodexEvent::Error {
                        message: format!("Failed to spawn Codex CLI: {}", e),
                        code: "cli_connection".to_string(),
                    })
                    .await;
                return;
            }
        };

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = tx
                    .send(CodexEvent::Error {
                        message: "No stdout pipe from Codex".to_string(),
                        code: "cli_connection".to_string(),
                    })
                    .await;
                let _ = child.kill().await;
                return;
            }
        };

        let stderr = child.stderr.take();
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!("codex stderr: {}", line);
                }
            });
        }

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if trimmed.is_empty() || !trimmed.starts_with('{') {
                continue;
            }
            match serde_json::from_str::<RawCodexEvent>(trimmed) {
                Ok(raw) => {
                    if let Some(event) = raw_to_event(raw) {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(CodexEvent::Error {
                            message: format!("JSON decode error: {}", e),
                            code: "json_decode".to_string(),
                        })
                        .await;
                }
            }
        }

        match child.wait().await {
            Ok(status) if !status.success() => {
                let message = match status.code() {
                    Some(code) => format!("Codex process exited with code {}", code),
                    None => "Codex process crashed".to_string(),
                };
                let _ = tx
                    .send(CodexEvent::Error {
                        message,
                        code: "process_error".to_string(),
                    })
                    .await;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx
                    .send(CodexEvent::Error {
                        message: e.to_string(),
                        code: "process_error".to_string(),
                    })
                    .await;
            }
        }
    });
    Ok(rx)
}

pub async fn run_session_turn(
    session: Arc<CodexSession>,
    prompt: String,
    _config: Arc<AppConfig>,
) -> Result<(), GatewayError> {
    let thread_id = session.thread_id.lock().await.clone();
    let result = run_command_collect(&prompt, &session.options, thread_id.as_deref()).await;

    match result {
        Ok(result) => {
            if let Some(thread_id) = result.thread_id.clone() {
                *session.thread_id.lock().await = Some(thread_id);
            }
            for event in result.events {
                let arc = Arc::new(event);
                let mut history = session.history.lock().await;
                history.push_back(arc.clone());
                while history.len() > MAX_CODEX_HISTORY_SIZE {
                    history.pop_front();
                }
                drop(history);
                let _ = session.event_tx.send(arc);
            }
            *session.state.lock().await = CodexSessionState::Idle;
            session
                .last_activity_ms
                .store(now_epoch_ms(), Ordering::Relaxed);
            Ok(())
        }
        Err(e) => {
            let event = Arc::new(CodexEvent::Error {
                message: e.to_string(),
                code: e.error_code().to_string(),
            });
            let mut history = session.history.lock().await;
            history.push_back(event.clone());
            while history.len() > MAX_CODEX_HISTORY_SIZE {
                history.pop_front();
            }
            drop(history);
            let _ = session.event_tx.send(event);
            *session.state.lock().await = CodexSessionState::Idle;
            Err(e)
        }
    }
}

pub fn path_display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::{effective_prompt, raw_to_event};
    use crate::codex::messages::{CodexEvent, RawCodexEvent};
    use serde_json::json;

    #[test]
    fn first_turn_includes_system_prompt() {
        let options = crate::codex::options::CodexOptions {
            system_prompt: Some("system text".to_string()),
            ..Default::default()
        };
        let prompt = effective_prompt("user text", &options, false);
        assert!(prompt.contains("<system>"));
        assert!(prompt.contains("system text"));
        assert!(prompt.contains("user text"));
    }

    #[test]
    fn resumed_turn_does_not_repeat_system_prompt() {
        let options = crate::codex::options::CodexOptions {
            system_prompt: Some("system text".to_string()),
            ..Default::default()
        };
        let prompt = effective_prompt("user text", &options, true);
        assert_eq!(prompt, "user text");
    }

    #[test]
    fn parses_command_event() {
        let raw: RawCodexEvent = serde_json::from_value(json!({
            "type": "item.completed",
            "item": {
                "id": "item_0",
                "type": "command_execution",
                "command": "/bin/zsh -lc pwd",
                "aggregated_output": "/tmp\n",
                "exit_code": 0,
                "status": "completed"
            }
        }))
        .unwrap();
        match raw_to_event(raw).unwrap() {
            CodexEvent::CommandExecution { item, completed } => {
                assert!(completed);
                assert_eq!(item.command, "/bin/zsh -lc pwd");
                assert_eq!(item.exit_code, Some(0));
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }
}
