use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("CLI not found at {path}: {detail}")]
    CliNotFound { path: String, detail: String },

    #[error("CLI connection failed: {0}")]
    CliConnection(String),

    #[error("CLI process exited with code {exit_code}: {stderr}")]
    ProcessExit { exit_code: i32, stderr: String },

    #[error("CLI process crashed (no exit code): {detail}")]
    ProcessCrash { detail: String },

    #[error("JSON decode error for line `{line}`: {source}")]
    JsonDecode { line: String, source: serde_json::Error },

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Session in wrong state: expected {expected}, got {actual}")]
    InvalidSessionState { expected: String, actual: String },

    #[error("Hook timeout (hook_id={hook_id})")]
    HookTimeout { hook_id: String },

    #[error("Concurrent session limit reached (max={max})")]
    SessionLimitReached { max: usize },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl GatewayError {
    pub fn http_status(&self) -> u16 {
        match self {
            Self::CliNotFound { .. } => 503,
            Self::CliConnection(_) | Self::ProcessExit { .. } | Self::ProcessCrash { .. } => 502,
            Self::JsonDecode { .. } => 502,
            Self::SessionNotFound(_) => 404,
            Self::InvalidSessionState { .. } => 409,
            Self::SessionLimitReached { .. } => 429,
            Self::HookTimeout { .. } => 408,
            _ => 500,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match self {
            Self::CliNotFound { .. } => "cli_not_found",
            Self::CliConnection(_) => "cli_connection",
            Self::ProcessExit { .. } | Self::ProcessCrash { .. } => "process_error",
            Self::JsonDecode { .. } => "json_decode",
            Self::SessionNotFound(_) => "session_not_found",
            Self::InvalidSessionState { .. } => "invalid_state",
            Self::SessionLimitReached { .. } => "rate_limited",
            Self::HookTimeout { .. } => "hook_timeout",
            _ => "internal_error",
        }
    }
}

/// JSON error response body
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl From<&GatewayError> for ErrorResponse {
    fn from(e: &GatewayError) -> Self {
        Self {
            error: ErrorBody {
                code: e.error_code().to_string(),
                message: e.to_string(),
            },
        }
    }
}
