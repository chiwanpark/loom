use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};
use loom_config::{
    Dirs, HumanDuration, LogLevel, ORACLE_CONFIG_ENV, OracleConfig, oracle_config_schema,
};
use loom_crypto::HostIdentity;
use loom_oracle::{Oracle, open_store, pairing_link, pairing_ttl, serve, serve_tls};
use loom_oracle_proto::Role;
use loom_relay::{TlsListener, load_tls};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "loom-oracle", version = loom_version::VERSION, about = "Loom-Next oracle")]
struct Cli {
    #[arg(long, global = true, env = ORACLE_CONFIG_ENV)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    #[command(subcommand)]
    Config(ConfigCommand),
    Pair {
        #[arg(long, default_value = "10m", value_parser = parse_duration)]
        ttl: Duration,
        #[arg(long)]
        no_wait: bool,
        #[arg(long)]
        no_qr: bool,
    },
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

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<HumanDuration>()
        .map(|d| d.as_duration())
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("loom-oracle: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let dirs = Dirs::from_env()?;
    let path = cli
        .config
        .map(|p| dirs.expand(&p))
        .unwrap_or_else(|| OracleConfig::default_path(&dirs));
    match cli.command.unwrap_or(Command::Serve) {
        Command::Config(ConfigCommand::Path) => println!("{}", path.display()),
        Command::Config(ConfigCommand::Schema) => {
            println!("{}", serde_json::to_string_pretty(&oracle_config_schema())?)
        }
        Command::Config(ConfigCommand::Check) => {
            OracleConfig::load(&path, &dirs)?;
            println!("{}: ok", path.display());
        }
        Command::Config(ConfigCommand::Init { force }) => {
            if path.exists() && !force {
                anyhow::bail!("{} exists; pass --force to overwrite it", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, OracleConfig::default().to_yaml())?;
            println!("wrote {}", path.display());
        }
        Command::Pair {
            ttl,
            no_wait,
            no_qr,
        } => {
            let config = OracleConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            return pair(&config, &dirs, ttl, no_wait, no_qr);
        }
        Command::Serve => {
            let config = OracleConfig::load(&path, &dirs)?;
            init_logging(config.logging.level);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(serve_main(config, dirs))?;
        }
    }
    Ok(ExitCode::SUCCESS)
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

fn pair(
    config: &OracleConfig,
    dirs: &Dirs,
    ttl: Duration,
    no_wait: bool,
    no_qr: bool,
) -> anyhow::Result<ExitCode> {
    let ttl = pairing_ttl(Some(ttl.as_secs())).map_err(|e| anyhow::anyhow!(e.message))?;
    let store = open_store(config, dirs)?;
    let data_dir = config.data_dir(dirs);
    let identity = HostIdentity::load_or_create(&data_dir)
        .with_context(|| format!("cannot read the keys in {}", data_dir.display()))?;
    if config.url.is_none() {
        eprintln!(
            "oracle.yaml sets no url, so the link uses {}",
            config.public_url()
        );
    }
    let (row, secret) = store.create_pairing(Role::Device, ttl, None)?;
    store.audit(
        None,
        "pairing_created",
        &format!("device pairing {} from loom-oracle pair", row.id),
    )?;
    let link = pairing_link(&config.public_url(), &identity, &secret);
    if !no_qr && let Ok(code) = qrcode::QrCode::new(link.as_bytes()) {
        let image = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{image}");
    }
    println!("{link}");
    println!(
        "This link pairs one device. It works once and expires in {}.",
        HumanDuration(ttl)
    );
    if no_wait {
        return Ok(ExitCode::SUCCESS);
    }
    let _ = std::io::stdout().flush();
    let deadline = Instant::now() + ttl;
    while Instant::now() < deadline {
        if !store.is_pending(&row.id)? {
            match store.peer_by_pairing(&row.id)? {
                Some(peer) => println!("paired with {}", peer.label),
                None => println!("the pairing link was used"),
            }
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("the pairing link expired");
    Ok(ExitCode::FAILURE)
}

async fn serve_main(config: OracleConfig, dirs: Dirs) -> anyhow::Result<()> {
    let addr = config.relay_config().listen_addr()?;
    let handshake = config.limits.handshake_timeout.as_duration();
    if config.cloudflare_access.is_none() {
        tracing::warn!("cloudflare_access is not set, so Access tokens are not checked");
    }
    let oracle = Oracle::open(&config, &dirs)?;
    tracing::info!(
        url = %oracle.url(),
        host = %oracle.identity().host_id(),
        data_dir = %config.data_dir(&dirs).display(),
        "starting the oracle"
    );
    if let Some(addr) = config.metrics_listen.clone() {
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .with_context(|| format!("cannot listen on {addr}"))?;
        tracing::info!(%addr, "serving metrics");
        let app = loom_oracle::metrics_router(oracle.clone());
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
    match &config.tls {
        Some(tls) => {
            let acceptor = load_tls(&tls.cert, &tls.key)?;
            tracing::info!(%addr, "listening with TLS");
            serve_tls(
                oracle,
                TlsListener::new(listener, acceptor, handshake)?,
                shutdown,
            )
            .await?;
        }
        None => {
            tracing::info!(%addr, "listening");
            serve(oracle, listener, shutdown).await?;
        }
    }
    Ok(())
}
