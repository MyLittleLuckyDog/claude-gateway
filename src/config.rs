use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_server_config")]
    pub server: ServerConfig,
    #[serde(default = "default_cli_config")]
    pub cli: CliConfig,
    #[serde(default = "default_proxy_config")]
    pub proxy: ProxyConfig,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    /// Max concurrent API requests (default: 1, conservative)
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Max proxy sessions (default: 50)
    #[serde(default = "default_max_proxy_sessions")]
    pub max_proxy_sessions: usize,
    /// Session idle timeout in seconds (default: 1800 = 30min)
    #[serde(default = "default_proxy_idle_timeout")]
    pub session_idle_timeout_secs: u64,
    /// Enable proxy mode (default: true)
    #[serde(default = "default_proxy_enabled")]
    pub enabled: bool,
}

fn default_proxy_config() -> ProxyConfig {
    ProxyConfig {
        max_concurrent: default_max_concurrent(),
        max_proxy_sessions: default_max_proxy_sessions(),
        session_idle_timeout_secs: default_proxy_idle_timeout(),
        enabled: default_proxy_enabled(),
    }
}

fn default_max_concurrent() -> usize { 1 }
fn default_max_proxy_sessions() -> usize { 50 }
fn default_proxy_idle_timeout() -> u64 { 1800 }
fn default_proxy_enabled() -> bool { true }

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
            proxy: default_proxy_config(),
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
