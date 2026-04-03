use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_server_config")]
    pub server: ServerConfig,
    #[serde(default = "default_cli_config")]
    pub cli: CliConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliConfig {
    #[serde(default = "default_bin_path")]
    pub bin_path: String,
    #[serde(default = "default_idle_timeout")]
    pub session_idle_timeout_secs: u64,
}

fn default_server_config() -> ServerConfig {
    ServerConfig {
        host: default_host(),
        port: default_port(),
        max_sessions: default_max_sessions(),
    }
}

fn default_cli_config() -> CliConfig {
    CliConfig {
        bin_path: default_bin_path(),
        session_idle_timeout_secs: default_idle_timeout(),
    }
}

fn default_host() -> String { "127.0.0.1".to_string() }
fn default_port() -> u16 { 8765 }
fn default_max_sessions() -> usize { 100 }
fn default_bin_path() -> String { String::new() }
fn default_idle_timeout() -> u64 { 1800 }

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: default_server_config(),
            cli: default_cli_config(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, config::ConfigError> {
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(config::Environment::with_prefix("CLAUDE_GATEWAY").separator("__"));

        let cfg = builder.build()?;
        cfg.try_deserialize()
    }
}
