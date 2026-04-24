use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeAgentOptions {
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub cli_path: Option<PathBuf>,

    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,

    #[serde(default)]
    pub permission_mode: Option<PermissionMode>,

    #[serde(default)]
    pub mcp_servers: Option<HashMap<String, McpServerConfig>>,

    #[serde(default)]
    pub resume: Option<String>,
    #[serde(default)]
    pub continue_conversation: bool,
    #[serde(default)]
    pub fork_session: Option<String>,

    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_budget_usd: Option<f64>,

    #[serde(default)]
    pub hook_rules: Option<Vec<HookRule>>,

    #[serde(default)]
    pub agents: Option<HashMap<String, AgentDefinition>>,

    #[serde(default)]
    pub include_partial_messages: bool,
    #[serde(default)]
    pub output_format: Option<serde_json::Value>,

    #[serde(default)]
    pub betas: Option<Vec<String>>,
    #[serde(default)]
    pub setting_sources: Option<Vec<String>>,
    #[serde(default)]
    pub add_dirs: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    #[serde(rename = "dontAsk")]
    DontAsk,
}

impl PermissionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::DontAsk => "dontAsk",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        env: Option<HashMap<String, String>>,
    },
    Sse {
        url: String,
        headers: Option<HashMap<String, String>>,
    },
    Http {
        url: String,
        headers: Option<HashMap<String, String>>,
    },
    Builtin {
        handler_name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRule {
    pub event: String,
    pub tool_pattern: Option<String>,
    pub action: HookAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Approve,
    Block { reason: Option<String> },
    Defer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub description: Option<String>,
    pub prompt: Option<String>,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
}
