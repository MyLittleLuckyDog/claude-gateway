use std::collections::HashMap;
use std::path::PathBuf;

use crate::error::GatewayError;
use crate::options::McpServerConfig;

/// Create a temporary MCP config file for the --mcp-config CLI flag.
/// Returns the path to the created temp file.
pub fn create_mcp_config_file(
    servers: &HashMap<String, McpServerConfig>,
) -> Result<PathBuf, GatewayError> {
    let mut mcp_servers = serde_json::Map::new();

    for (name, config) in servers {
        let server_obj = match config {
            McpServerConfig::Stdio { command, args, env } => {
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "command".to_string(),
                    serde_json::Value::String(command.clone()),
                );
                obj.insert(
                    "args".to_string(),
                    serde_json::to_value(args)
                        .map_err(|e| GatewayError::Internal(e.to_string()))?,
                );
                if let Some(env) = env {
                    obj.insert(
                        "env".to_string(),
                        serde_json::to_value(env)
                            .map_err(|e| GatewayError::Internal(e.to_string()))?,
                    );
                }
                serde_json::Value::Object(obj)
            }
            McpServerConfig::Sse { url, headers } => {
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String("sse".to_string()),
                );
                obj.insert("url".to_string(), serde_json::Value::String(url.clone()));
                if let Some(headers) = headers {
                    obj.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .map_err(|e| GatewayError::Internal(e.to_string()))?,
                    );
                }
                serde_json::Value::Object(obj)
            }
            McpServerConfig::Http { url, headers } => {
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "type".to_string(),
                    serde_json::Value::String("http".to_string()),
                );
                obj.insert("url".to_string(), serde_json::Value::String(url.clone()));
                if let Some(headers) = headers {
                    obj.insert(
                        "headers".to_string(),
                        serde_json::to_value(headers)
                            .map_err(|e| GatewayError::Internal(e.to_string()))?,
                    );
                }
                serde_json::Value::Object(obj)
            }
            McpServerConfig::Builtin { .. } => continue, // handled internally
        };
        mcp_servers.insert(name.clone(), server_obj);
    }

    let config = serde_json::json!({
        "mcpServers": mcp_servers
    });

    let dir = std::env::temp_dir().join("claude-agent-rs");
    std::fs::create_dir_all(&dir).map_err(GatewayError::Io)?;

    let filename = format!("mcp-{}.json", uuid::Uuid::new_v4());
    let path = dir.join(filename);

    let json_str =
        serde_json::to_string_pretty(&config).map_err(|e| GatewayError::Internal(e.to_string()))?;
    std::fs::write(&path, json_str).map_err(GatewayError::Io)?;

    tracing::debug!("Created MCP config file at {:?}", path);
    Ok(path)
}

/// Clean up a temp MCP config file.
pub fn cleanup_mcp_config_file(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!("Failed to cleanup MCP config file {:?}: {}", path, e);
    }
}
