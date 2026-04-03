use std::sync::Arc;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use claude_agent::api::{self, AppState, Stats};
use claude_agent::config::AppConfig;
use claude_agent::session::store::SessionStore;

#[derive(Parser)]
#[command(name = "claude-agent-rs", about = "Claude Code CLI REST API Gateway")]
struct Cli {
    #[arg(long, help = "Check CLI availability and exit")]
    check_cli: bool,

    #[arg(long, default_value = "8765", help = "Port to listen on")]
    port: Option<u16>,

    #[arg(long, default_value = "127.0.0.1", help = "Host to bind")]
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

    // Spawn cleanup task
    let cleanup_sessions = sessions.clone();
    let cleanup_config = config.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            cleanup_sessions.run_cleanup(cleanup_config.cli.session_idle_timeout_secs).await;
        }
    });

    let state = AppState {
        config: config.clone(),
        sessions,
        start_time: std::time::Instant::now(),
        stats: Arc::new(tokio::sync::Mutex::new(Stats::default())),
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
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
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
