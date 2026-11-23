use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::bail;
use clap::{Parser, Subcommand};
use loom_config::{Dirs, HOST_CONFIG_ENV, HostConfig, LogLevel, host_config_schema};
use loom_host::{CheckStatus, JoinOptions, JoinedOracle, join};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "loom-host", version = loom_version::VERSION, about = "Loom-Next host daemon")]
struct Cli {
    #[arg(long, global = true, env = HOST_CONFIG_ENV)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Join {
        link: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, requires = "access_secret_env")]
        access_id_env: Option<String>,
        #[arg(long, requires = "access_id_env")]
        access_secret_env: Option<String>,
        #[arg(long)]
        force: bool,
    },
    Serve,
    Check,
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
        Ok(code) => code,
        Err(err) => {
            eprintln!("loom-host: {err:#}");
            ExitCode::FAILURE
        }
    }
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

fn default_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| loom_crypto::validate_label(h).is_ok())
        .unwrap_or_else(|| "host".to_owned())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let dirs = Dirs::from_env()?;
    let path = cli
        .config
        .map(|p| dirs.expand(&p))
        .unwrap_or_else(|| HostConfig::default_path(&dirs));
    match cli.command {
        Command::Config(ConfigCommand::Path) => println!("{}", path.display()),
        Command::Config(ConfigCommand::Schema) => {
            println!("{}", serde_json::to_string_pretty(&host_config_schema())?)
        }
        Command::Config(ConfigCommand::Check) => {
            HostConfig::load(&path, &dirs)?;
            println!("{}: ok", path.display());
        }
        Command::Config(ConfigCommand::Init { force }) => {
            if path.exists() && !force {
                bail!("{} exists; pass --force to overwrite it", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, HostConfig::default().to_yaml())?;
            println!("wrote {}", path.display());
        }
        Command::Join {
            link,
            name,
            access_id_env,
            access_secret_env,
            force,
        } => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            let name = name.unwrap_or_else(default_name);
            loom_crypto::validate_label(&name)?;
            let joined = runtime()?.block_on(join(
                &config.data_dir(&dirs),
                JoinOptions {
                    link,
                    name,
                    access_id_env,
                    access_secret_env,
                    force,
                },
            ))?;
            println!(
                "joined the oracle {} as the host {}",
                joined.url, joined.name
            );
            println!("Now run `loom-host serve`, for example as a systemd service.");
        }
        Command::Check => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            return runtime()?.block_on(check(config, dirs));
        }
        Command::Serve => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(config.logging.level);
            runtime()?.block_on(serve(config, dirs))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn check(config: HostConfig, dirs: Dirs) -> anyhow::Result<ExitCode> {
    let data_dir = config.data_dir(&dirs);
    let mut failed = false;
    match JoinedOracle::load(&data_dir) {
        Ok(oracle) => println!("ok\toracle\t{} as {}", oracle.url, oracle.name),
        Err(err) => {
            failed = true;
            println!("failed\toracle\t{err:#}");
        }
    }
    match loom_host::agents(&config).agent(None) {
        Ok(agent) => println!("ok\tagent\t{}", agent.path.display()),
        Err(err) => {
            failed = true;
            println!("failed\tagent\t{err:#}");
        }
    }
    let driver = loom_host::driver(&config, &dirs)?;
    match driver.report().await {
        Ok(report) => println!(
            "ok\treport\t{} driver, {} CPUs, {} MiB",
            report.driver, report.cpus, report.memory_mib
        ),
        Err(err) => {
            failed = true;
            println!("failed\treport\t{err:#}");
        }
    }
    for item in driver.check().await {
        let status = match item.status {
            CheckStatus::Ok => "ok",
            CheckStatus::Warning => "warning",
            CheckStatus::Failed => {
                failed = true;
                "failed"
            }
        };
        println!("{status}\t{}\t{}", item.name, item.detail);
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn serve(config: HostConfig, dirs: Dirs) -> anyhow::Result<()> {
    let host = loom_host::host(config, &dirs)?;
    tracing::info!(
        oracle = %host.oracle.url,
        name = %host.oracle.name,
        "starting the host"
    );
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
    host.run(shutdown).await
}
