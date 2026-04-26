use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTurnUsage {
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexCommandItem {
    pub id: String,
    pub command: String,
    pub aggregated_output: String,
    pub exit_code: Option<i32>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexMessageItem {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CodexEvent {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted,
    CommandExecution {
        item: CodexCommandItem,
        completed: bool,
    },
    AgentMessage {
        item: CodexMessageItem,
        completed: bool,
    },
    TurnCompleted {
        usage: CodexTurnUsage,
    },
    Error {
        message: String,
        code: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexQueryResult {
    pub thread_id: Option<String>,
    pub output_text: Option<String>,
    pub usage: Option<CodexTurnUsage>,
    pub events: Vec<CodexEvent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum RawCodexEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: CodexTurnUsage },
    #[serde(rename = "item.started")]
    ItemStarted { item: CodexItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum CodexItem {
    #[serde(rename = "command_execution")]
    CommandExecution(RawCodexCommandItem),
    #[serde(rename = "agent_message")]
    AgentMessage(RawCodexMessageItem),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawCodexCommandItem {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub aggregated_output: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawCodexMessageItem {
    pub id: String,
    pub text: String,
}

impl From<RawCodexCommandItem> for CodexCommandItem {
    fn from(value: RawCodexCommandItem) -> Self {
        Self {
            id: value.id,
            command: value.command,
            aggregated_output: value.aggregated_output,
            exit_code: value.exit_code,
            status: value.status,
        }
    }
}

impl From<RawCodexMessageItem> for CodexMessageItem {
    fn from(value: RawCodexMessageItem) -> Self {
        Self {
            id: value.id,
            text: value.text,
        }
    }
}
