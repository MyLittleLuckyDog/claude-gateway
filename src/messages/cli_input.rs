use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliInputMessage {
    User { message: CliUserInput },
    Interrupt,
}

#[derive(Debug, Serialize)]
pub struct CliUserInput {
    pub role: String,
    pub content: Vec<InputContent>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Debug, Serialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Approve,
    Block,
    Defer,
}
