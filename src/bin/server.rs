use std::sync::Arc;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use claude_agent::api::{self, AppState, Stats};
use claude_agent::config::AppConfig;
use claude_agent::codex::store::CodexSessionStore;
use claude_agent::codex_app::store::CodexAppSessionStore;
use claude_agent::openai_proxy::OpenAiProxyState;
use claude_agent::session::store::SessionStore;

#[derive(Parser)]
#[command(name = "claude-agent-rs", about = "Claude Code CLI REST API Gateway")]
struct Cli {
    #[arg(long, help = "Check CLI availability and exit")]
    check_cli: bool,

    #[arg(long, help = "Port to listen on")]
    port: Option<u16>,

    #[arg(long, help = "Host to bind")]
    host: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = Cli::parse();

    if cli.check_cli {
        check_cli_available();
        return Ok(());
    }

    let mut config = AppConfig::load().unwrap_or_default();

    // CLI args override config
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(host) = cli.host {
        config.server.host = host;
    }

    let config = Arc::new(config);
    let sessions = Arc::new(SessionStore::new(config.server.max_sessions));
    let codex_sessions = Arc::new(CodexSessionStore::new(config.server.max_sessions));
    let codex_app_sessions = Arc::new(CodexAppSessionStore::new(config.server.max_sessions));

    // Spawn cleanup task
    let cleanup_sessions = sessions.clone();
    let cleanup_config = config.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            cleanup_sessions.run_cleanup(cleanup_config.cli.session_idle_timeout_secs).await;
        }
    });

    let cleanup_codex_sessions = codex_sessions.clone();
    let cleanup_codex_config = config.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            cleanup_codex_sessions
                .run_cleanup(cleanup_codex_config.cli.session_idle_timeout_secs)
                .await;
        }
    });

    let cleanup_codex_app_sessions = codex_app_sessions.clone();
    let cleanup_codex_app_config = config.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            cleanup_codex_app_sessions
                .run_cleanup(cleanup_codex_app_config.cli.session_idle_timeout_secs)
                .await;
        }
    });

    // Initialize direct API proxy
    let proxy = if config.proxy.enabled {
        match claude_agent::auth::get_oauth_token() {
            Ok(token) => {
                tracing::info!(
                    "OAuth token loaded (subscription: {:?}, tier: {:?})",
                    token.subscription_type, token.rate_limit_tier
                );
                Some(Arc::new(claude_agent::proxy::ProxyState::new(
                    config.proxy.max_concurrent,
                )))
            }
            Err(e) => {
                tracing::warn!("Direct API proxy disabled: {}", e);
                None
            }
        }
    } else {
        tracing::info!("Direct API proxy disabled by config");
        None
    };

    // Run quota pre-check if proxy is enabled
    if let Some(ref ps) = proxy {
        claude_agent::proxy::check_quota_at_startup(ps).await;
    }

    let proxy_sessions = proxy.as_ref().map(|_| {
        Arc::new(claude_agent::proxy_session::ProxySessionStore::new(
            config.proxy.max_proxy_sessions,
            config.proxy.session_idle_timeout_secs,
        ))
    });

    let openai = OpenAiProxyState::from_env();
    if openai.is_some() {
        tracing::info!("OpenAI Responses proxy enabled");
    } else {
        tracing::info!("OpenAI Responses proxy disabled (OPENAI_API_KEY not set)");
    }

    let openai_oauth = claude_agent::openai_oauth::OpenAiOAuthState::from_auth_file().await;
    if openai_oauth.is_some() {
        tracing::info!("OpenAI OAuth channel enabled");
    } else {
        tracing::info!("OpenAI OAuth channel disabled (auth.json not found)");
    }

    // Spawn proxy session cleanup task
    if let Some(ref ps) = proxy_sessions {
        let cleanup_store = ps.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let removed = cleanup_store.cleanup().await;
                if removed > 0 {
                    tracing::info!("Cleaned up {} idle proxy session(s)", removed);
                }
            }
        });
    }

    let state = AppState {
        config: config.clone(),
        sessions,
        codex_sessions,
        codex_app_sessions,
        start_time: std::time::Instant::now(),
        stats: Arc::new(tokio::sync::Mutex::new(Stats::default())),
        proxy,
        proxy_sessions,
        openai,
        openai_oauth,
    };

    let app = api::build_router(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    tracing::info!("Starting claude-agent-rs on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("Server shut down gracefully");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("Failed to install Ctrl+C handler: {e}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => { sig.recv().await; }
            Err(e) => {
                tracing::error!("Failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl+C, shutting down..."); }
        _ = terminate => { tracing::info!("Received SIGTERM, shutting down..."); }
    }
}

fn check_cli_available() {
    match std::process::Command::new("which").arg("claude").output() {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            println!("claude CLI found at {}", path);
        }
        _ => {
            eprintln!("claude CLI not found in PATH");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn cli_does_not_override_config_when_host_and_port_are_omitted() {
        let cli = Cli::parse_from(["claude-agent-rs"]);
        assert_eq!(cli.port, None);
        assert_eq!(cli.host, None);
    }

    #[test]
    fn cli_accepts_explicit_port_override() {
        let cli = Cli::parse_from(["claude-agent-rs", "--port", "8876"]);
        assert_eq!(cli.port, Some(8876));
    }
}
