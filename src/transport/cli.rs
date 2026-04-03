use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use async_trait::async_trait;

use crate::config::AppConfig;
use crate::error::GatewayError;
use crate::messages::cli_output::CliOutputEvent;
use crate::options::ClaudeAgentOptions;

use super::Transport;

pub struct CliTransport {
    child: Option<Child>,
    stdin_tx: Option<mpsc::Sender<String>>,
    event_rx: Option<mpsc::Receiver<Result<CliOutputEvent, GatewayError>>>,
    cli_session_id: Option<String>,
    mcp_config_path: Option<PathBuf>,
    options: ClaudeAgentOptions,
    config: AppConfig,
}

impl CliTransport {
    pub fn new(options: ClaudeAgentOptions, config: AppConfig) -> Self {
        Self {
            child: None,
            stdin_tx: None,
            event_rx: None,
            cli_session_id: None,
            mcp_config_path: None,
            options,
            config,
        }
    }

    fn build_command(options: &ClaudeAgentOptions, config: &AppConfig) -> Command {
        let cli_path = options.cli_path.clone()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                if config.cli.bin_path.is_empty() {
                    "claude".to_string()
                } else {
                    config.cli.bin_path.clone()
                }
            });

        let mut cmd = Command::new(&cli_path);
        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--input-format").arg("stream-json");
        cmd.arg("--verbose"); // required by stream-json

        if let Some(model) = &options.model {
            cmd.arg("--model").arg(model);
        }

        let perm_mode = options.permission_mode.as_ref()
            .map(|m| m.as_str())
            .unwrap_or("default");
        cmd.arg("--permission-mode").arg(perm_mode);

        if let Some(sp) = &options.system_prompt {
            cmd.arg("--system-prompt").arg(sp);
        }

        if let Some(session_id) = &options.resume {
            cmd.arg("--resume").arg(session_id);
        } else if options.continue_conversation {
            cmd.arg("--continue");
        }

        if let Some(max) = options.max_turns {
            cmd.arg("--max-turns").arg(max.to_string());
        }

        if let Some(tools) = &options.allowed_tools {
            cmd.arg("--allowedTools").arg(tools.join(","));
        }

        if let Some(tools) = &options.disallowed_tools {
            cmd.arg("--disallowedTools").arg(tools.join(","));
        }

        if let Some(betas) = &options.betas {
            for beta in betas {
                cmd.arg("--beta").arg(beta);
            }
        }

        if let Some(sources) = &options.setting_sources {
            cmd.arg("--setting-sources").arg(sources.join(","));
        } else {
            cmd.arg("--setting-sources").arg("");
        }

        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }

        if let Some(env) = &options.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        cmd.stdin(Stdio::piped())
           .stdout(Stdio::piped())
           .stderr(Stdio::piped())
           .kill_on_drop(true);

        cmd
    }

    pub fn set_session_id(&mut self, id: String) {
        self.cli_session_id = Some(id);
    }
}

#[async_trait]
impl Transport for CliTransport {
    async fn connect(&mut self) -> Result<(), GatewayError> {
        let mut cmd = Self::build_command(&self.options, &self.config);

        // MCP config: create temp JSON file and add --mcp-config flag
        if let Some(ref servers) = self.options.mcp_servers {
            if !servers.is_empty() {
                let path = crate::mcp::config_file::create_mcp_config_file(servers)?;
                cmd.arg("--mcp-config").arg(&path);
                self.mcp_config_path = Some(path);
            }
        }

        tracing::debug!("spawning CLI: {:?}", cmd.as_std());
        let mut child = cmd.spawn().map_err(|e| {
            GatewayError::CliConnection(format!("Failed to spawn CLI: {}", e))
        })?;
        tracing::debug!("CLI process spawned, pid={:?}", child.id());

        let stdout = child.stdout.take()
            .ok_or_else(|| GatewayError::CliConnection("No stdout pipe".to_string()))?;
        let stdin = child.stdin.take()
            .ok_or_else(|| GatewayError::CliConnection("No stdin pipe".to_string()))?;
        let stderr = child.stderr.take();

        // stdout parser
        let (event_tx, event_rx) = mpsc::channel::<Result<CliOutputEvent, GatewayError>>(256);
        tokio::spawn(stdout_parser_task(stdout, event_tx));

        // stdin writer
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(32);
        tokio::spawn(stdin_writer_task(stdin, stdin_rx));

        // stderr logger
        if let Some(stderr) = stderr {
            tokio::spawn(stderr_logger_task(stderr));
        }

        self.child = Some(child);
        self.stdin_tx = Some(stdin_tx);
        self.event_rx = Some(event_rx);

        Ok(())
    }

    async fn write(&self, data: &str) -> Result<(), GatewayError> {
        let tx = self.stdin_tx.as_ref()
            .ok_or_else(|| GatewayError::Internal("Transport not connected".to_string()))?;
        tx.send(data.to_string()).await
            .map_err(|_| GatewayError::Internal("stdin channel closed".to_string()))
    }

    async fn close(&mut self) -> Result<(), GatewayError> {
        // Drop stdin to signal EOF
        self.stdin_tx.take();

        if let Some(ref mut child) = self.child {
            // Wait up to 3 seconds for graceful exit
            let wait_result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                child.wait(),
            ).await;

            match wait_result {
                Ok(Ok(_status)) => {},
                Ok(Err(e)) => {
                    tracing::warn!("Error waiting for CLI process: {}", e);
                }
                Err(_) => {
                    // Timeout — kill
                    tracing::warn!("CLI process did not exit in 3s, killing");
                    let _ = child.kill().await;
                }
            }
        }

        self.child = None;

        // Cleanup MCP config temp file
        if let Some(ref path) = self.mcp_config_path {
            crate::mcp::config_file::cleanup_mcp_config_file(path);
        }
        self.mcp_config_path = None;

        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.stdin_tx.is_some()
    }

    fn session_id(&self) -> Option<&str> {
        self.cli_session_id.as_deref()
    }

    fn event_receiver(&mut self) -> Option<mpsc::Receiver<Result<CliOutputEvent, GatewayError>>> {
        self.event_rx.take()
    }
}

async fn stdout_parser_task(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::Sender<Result<CliOutputEvent, GatewayError>>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() || !trimmed.starts_with('{') {
                    tracing::debug!("cli non-json stdout: {}", trimmed);
                    continue;
                }

                let event = serde_json::from_str::<CliOutputEvent>(&trimmed)
                    .map_err(|e| GatewayError::JsonDecode {
                        line: trimmed,
                        source: e,
                    });

                if event_tx.send(event).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::error!("stdout read error: {}", e);
                break;
            }
        }
    }
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

        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            tracing::error!("stdin write error: {}", e);
            break;
        }
        if let Err(e) = stdin.flush().await {
            tracing::error!("stdin flush error: {}", e);
            break;
        }
    }
}

async fn stderr_logger_task(stderr: tokio::process::ChildStderr) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();

    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                tracing::warn!("cli stderr: {}", line);
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
}
