use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use loom_config::{Dirs, LogLevel, RELAY_CONFIG_ENV, RelayServerConfig, relay_config_schema};
use loom_relay::{Relay, TlsListener, load_tls, serve, serve_tls};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "loom-relay", version, about = "Loom relay server")]
struct Cli {
    #[arg(long, global = true, env = RELAY_CONFIG_ENV)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Subcommand)]
enum ConfigCommand {
    Init {
        #[arg(long)]
        force: bool,
    },
    Check,
    Schema,
    Path,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("loom-relay: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let dirs = Dirs::from_env()?;
    let path = cli
        .config
        .map(|p| dirs.expand(&p))
        .unwrap_or_else(|| RelayServerConfig::default_path(&dirs));
    match cli.command.unwrap_or(Command::Serve) {
        Command::Config(ConfigCommand::Path) => println!("{}", path.display()),
        Command::Config(ConfigCommand::Schema) => {
            println!("{}", serde_json::to_string_pretty(&relay_config_schema())?)
        }
        Command::Config(ConfigCommand::Check) => {
            RelayServerConfig::load(&path, &dirs)?;
            println!("{}: ok", path.display());
        }
        Command::Config(ConfigCommand::Init { force }) => {
            if path.exists() && !force {
                anyhow::bail!("{} exists; pass --force to overwrite it", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, RelayServerConfig::default().to_yaml())?;
            println!("wrote {}", path.display());
        }
        Command::Serve => {
            let config = RelayServerConfig::load(&path, &dirs)?;
            init_logging(config.logging.level);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve_main(config))?;
        }
    }
    Ok(())
}

fn init_logging(level: LogLevel) {
    let default = format!("{level:?}").to_lowercase();
    let filter = tracing_subscriber::EnvFilter::try_from_env("LOOM_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

async fn serve_main(config: RelayServerConfig) -> anyhow::Result<()> {
    let addr = config.listen_addr()?;
    let tls = config.tls.clone();
    let handshake = config.limits.handshake_timeout.as_duration();
    if config.allowed_hosts.is_empty() {
        tracing::warn!("allowed_hosts is empty, so no agent can register");
    }
    if config.cloudflare_access.is_none() {
        tracing::warn!("cloudflare_access is not set, so Access tokens are not checked");
    }
    let metrics_addr = config.metrics_listen.clone();
    let relay = Relay::new(config);
    if let Some(addr) = metrics_addr {
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        tracing::info!(%addr, "serving metrics");
        let app = relay.metrics_router();
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, app).await {
                tracing::error!(error = %err, "the metrics server stopped");
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
        }
        signal.cancel();
    });
    match tls {
        Some(tls) => {
            let acceptor = load_tls(&tls.cert, &tls.key)?;
            tracing::info!(%addr, "listening with TLS");
            serve_tls(
                relay,
                TlsListener::new(listener, acceptor, handshake)?,
                shutdown,
            )
            .await?;
        }
        None => {
            tracing::info!(%addr, "listening");
            serve(relay, listener, shutdown).await?;
        }
    }
    Ok(())
}
