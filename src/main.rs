mod error;
mod mcp;
mod server;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "claude-notify", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the notification server
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve => serve().await,
    }
}

async fn serve() -> anyhow::Result<()> {
    let config =
        server::config::ServerConfig::from_env().context("failed to load server config")?;
    let listen_addr = config.listen_addr.clone();

    let state = server::AppState::new(config);

    // Spawn session eviction background task
    let sessions = Arc::clone(&state.sessions);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            sessions.evict_stale().await;
        }
    });

    let app = server::router(state);

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .context(format!("failed to bind {listen_addr}"))?;

    info!(addr = listen_addr, "server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install ctrl+c handler");
    info!("shutting down");
}
