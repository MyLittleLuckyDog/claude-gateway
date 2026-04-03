pub mod builtin;
pub mod config_file;

use std::collections::HashMap;

use crate::options::McpServerConfig;

/// Tracks MCP server status.
#[derive(Debug, Clone)]
pub struct McpManager {
    servers: HashMap<String, McpServerStatus>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub config_type: String,
    pub status: String,
}

impl McpManager {
    pub fn new(servers: &Option<HashMap<String, McpServerConfig>>) -> Self {
        let mut status_map = HashMap::new();
        if let Some(servers) = servers {
            for (name, config) in servers {
                let config_type = match config {
                    McpServerConfig::Stdio { .. } => "stdio",
                    McpServerConfig::Sse { .. } => "sse",
                    McpServerConfig::Http { .. } => "http",
                    McpServerConfig::Builtin { .. } => "builtin",
                };
                status_map.insert(
                    name.clone(),
                    McpServerStatus {
                        name: name.clone(),
                        config_type: config_type.to_string(),
                        status: "pending".to_string(),
                    },
                );
            }
        }
        Self {
            servers: status_map,
        }
    }

    pub fn list(&self) -> Vec<&McpServerStatus> {
        self.servers.values().collect()
    }

    pub fn update_status(&mut self, name: &str, status: &str) {
        if let Some(s) = self.servers.get_mut(name) {
            s.status = status.to_string();
        }
    }
}
