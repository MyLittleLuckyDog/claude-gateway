use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_server_config")]
    pub server: ServerConfig,
    #[serde(default = "default_cli_config")]
    pub cli: CliConfig,
    #[serde(default = "default_proxy_config")]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub request_options: RequestOptionsConfig,
}

/// How much of the session option surface a request may set.
///
/// See `core::request_policy`. The default is `trusted`, which is right for
/// the loopback gateway this ships as — and which the server refuses to start
/// with once it is reachable from anywhere else.
#[derive(Debug, Clone, Deserialize)]
pub struct RequestOptionsConfig {
    #[serde(default)]
    pub policy: crate::core::request_policy::PolicyMode,
    /// Directories a request may point `cwd`/`add_dirs` at, under `restricted`.
    /// Empty means a request may not choose one.
    #[serde(default)]
    pub allowed_roots: Vec<std::path::PathBuf>,
    /// Most permissive `permission_mode` a request may ask for, under
    /// `restricted`.
    #[serde(default = "default_max_permission_mode")]
    pub max_permission_mode: crate::options::PermissionMode,
    /// Most permissive Codex `sandbox` a request may ask for, under
    /// `restricted`.
    #[serde(default = "default_max_codex_sandbox")]
    pub max_codex_sandbox: crate::codex::options::CodexSandboxMode,
}

fn default_max_permission_mode() -> crate::options::PermissionMode {
    crate::options::PermissionMode::Plan
}

fn default_max_codex_sandbox() -> crate::codex::options::CodexSandboxMode {
    crate::codex::options::CodexSandboxMode::ReadOnly
}

impl Default for RequestOptionsConfig {
    fn default() -> Self {
        Self {
            policy: crate::core::request_policy::PolicyMode::default(),
            allowed_roots: Vec::new(),
            max_permission_mode: default_max_permission_mode(),
            max_codex_sandbox: default_max_codex_sandbox(),
        }
    }
}

impl RequestOptionsConfig {
    pub fn to_policy(&self) -> crate::core::request_policy::RequestPolicy {
        crate::core::request_policy::RequestPolicy {
            mode: self.policy,
            allowed_roots: self.allowed_roots.clone(),
            max_permission_mode: self.max_permission_mode.clone(),
            max_codex_sandbox: self.max_codex_sandbox.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Browser origins allowed to call the gateway.
    ///
    /// Entries are matched exactly, except that a bare host origin also
    /// matches any port on it (`http://localhost` covers
    /// `http://localhost:3000`). The scheme is part of the origin, so
    /// `https://` needs its own entry.
    ///
    /// Empty means no cross-origin caller is allowed. To serve any origin,
    /// set `cors_allow_any_origin` — that is deliberately something you have
    /// to type, not something a cleared list gives you by accident.
    #[serde(default = "default_cors_origins")]
    pub cors_origins: Vec<String>,
    /// Serve every origin, ignoring `cors_origins`.
    ///
    /// The gateway has no authentication of its own, so this hands any page
    /// the user visits the ability to spend their Claude subscription and —
    /// with `permission_mode: bypassPermissions` — run commands on this host.
    #[serde(default)]
    pub cors_allow_any_origin: bool,
    /// Let browsers send cookies and HTTP auth with cross-origin requests.
    ///
    /// Needed when something in front of the gateway authenticates with a
    /// cookie, which is the only credential `EventSource` can carry. Cannot be
    /// combined with `cors_allow_any_origin`: browsers reject credentialed
    /// responses that allow every origin.
    #[serde(default)]
    pub cors_allow_credentials: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliConfig {
    #[serde(default = "default_bin_path")]
    pub bin_path: String,
    #[serde(default = "default_idle_timeout")]
    pub session_idle_timeout_secs: u64,
}

fn default_cors_origins() -> Vec<String> {
    vec![
        "http://localhost".to_string(),
        "http://127.0.0.1".to_string(),
    ]
}

fn default_server_config() -> ServerConfig {
    ServerConfig {
        host: default_host(),
        port: default_port(),
        max_sessions: default_max_sessions(),
        cors_origins: default_cors_origins(),
        cors_allow_any_origin: false,
        cors_allow_credentials: false,
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

fn default_max_concurrent() -> usize {
    1
}
fn default_max_proxy_sessions() -> usize {
    50
}
fn default_proxy_idle_timeout() -> u64 {
    1800
}
fn default_proxy_enabled() -> bool {
    true
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8765
}
fn default_max_sessions() -> usize {
    100
}
fn default_bin_path() -> String {
    String::new()
}
fn default_idle_timeout() -> u64 {
    1800
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: default_server_config(),
            cli: default_cli_config(),
            proxy: default_proxy_config(),
            request_options: RequestOptionsConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Whether only this machine can reach the gateway.
    ///
    /// This is what makes `PolicyMode::Trusted` defensible: a loopback caller
    /// is the local user and gains nothing by asking for `cli_path` or
    /// `bypassPermissions` that it could not do directly.
    pub fn is_loopback_only(&self) -> bool {
        let bound_locally = matches!(self.host.as_str(), "127.0.0.1" | "::1" | "localhost")
            || self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());

        let browsers_are_local = !self.cors_allow_any_origin
            && self.cors_origins.iter().all(|o| {
                let o = o.trim();
                o.is_empty()
                    || o.starts_with("http://localhost")
                    || o.starts_with("http://127.0.0.1")
                    || o.starts_with("http://[::1]")
            });

        bound_locally && browsers_are_local
    }
}

impl AppConfig {
    /// Refuse configurations that hand the option surface to callers the
    /// operator cannot vouch for.
    ///
    /// Session options reach the CLI almost untouched, so `trusted` is only
    /// safe while the gateway is loopback-only. Rather than making that a flag
    /// someone forgets, it is checked against the exposure the rest of the
    /// config describes.
    pub fn check_exposure(&self) -> Result<(), String> {
        use crate::core::request_policy::PolicyMode;

        if self.request_options.policy != PolicyMode::Trusted || self.server.is_loopback_only() {
            return Ok(());
        }
        Err(format!(
            "request_options.policy is \"trusted\" but the gateway is reachable beyond this \
             machine (host = {}, cors_allow_any_origin = {}, cors_origins = {:?}).\n\
             A request can then choose the binary to run (options.cli_path), its environment \
             (options.env), spawn another process (options.mcp_servers) and skip the permission \
             prompt (options.permission_mode).\n\
             Set request_options.policy = \"restricted\" — see docs/USAGE.md.",
            self.server.host, self.server.cors_allow_any_origin, self.server.cors_origins,
        ))
    }

    /// Read `config.toml` (optional) overlaid with `CLAUDE_GATEWAY__*`.
    ///
    /// List-valued settings come from the environment comma-separated:
    ///
    /// ```text
    /// CLAUDE_GATEWAY__SERVER__CORS_ORIGINS=https://app.example.com,http://localhost
    /// ```
    ///
    /// Callers must not fall back to defaults on error. A setting the operator
    /// wrote and the gateway silently ignored is worse than not starting —
    /// especially for `cors_origins`, where the default is more permissive than
    /// anything someone would type by hand.
    pub fn load() -> Result<Self, config::ConfigError> {
        let builder = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(
                config::Environment::with_prefix("CLAUDE_GATEWAY")
                    .separator("__")
                    .try_parsing(true)
                    .list_separator(",")
                    .with_list_parse_key("server.cors_origins")
                    .with_list_parse_key("request_options.allowed_roots"),
            );

        let cfg = builder.build()?;
        cfg.try_deserialize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::request_policy::PolicyMode;

    fn cfg(host: &str, origins: &[&str], any: bool, policy: PolicyMode) -> AppConfig {
        AppConfig {
            server: ServerConfig {
                host: host.to_string(),
                cors_origins: origins.iter().map(|s| s.to_string()).collect(),
                cors_allow_any_origin: any,
                ..default_server_config()
            },
            request_options: RequestOptionsConfig {
                policy,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The shipped defaults must keep working with no config at all.
    #[test]
    fn the_default_loopback_configuration_starts() {
        assert!(AppConfig::default().check_exposure().is_ok());
    }

    #[test]
    fn binding_beyond_loopback_refuses_trusted_options() {
        let err = cfg("0.0.0.0", &["http://localhost"], false, PolicyMode::Trusted)
            .check_exposure()
            .unwrap_err();
        assert!(err.contains("restricted"), "{err}");
    }

    /// Pointing CORS at a real site exposes the option surface to a browser
    /// just as surely as binding a public interface does.
    #[test]
    fn a_public_cors_origin_refuses_trusted_options() {
        assert!(cfg(
            "127.0.0.1",
            &["https://app.example.com"],
            false,
            PolicyMode::Trusted
        )
        .check_exposure()
        .is_err());
    }

    #[test]
    fn allow_any_origin_refuses_trusted_options() {
        assert!(cfg("127.0.0.1", &[], true, PolicyMode::Trusted)
            .check_exposure()
            .is_err());
    }

    #[test]
    fn restricting_the_options_permits_the_same_exposure() {
        assert!(cfg(
            "0.0.0.0",
            &["https://app.example.com"],
            false,
            PolicyMode::Restricted
        )
        .check_exposure()
        .is_ok());
    }

    #[test]
    fn ipv6_loopback_counts_as_local() {
        assert!(
            cfg("::1", &["http://[::1]:5173"], false, PolicyMode::Trusted)
                .check_exposure()
                .is_ok()
        );
    }
}
