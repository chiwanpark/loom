use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use loom_config::{Dirs, HOST_CONFIG_ENV, HostConfig, HostDriver, LogLevel, host_config_schema};
use loom_host::{CheckStatus, JoinOptions, JoinedOracle, SetupResult, join};
use loom_incus::TrustToken;
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
    Serve {
        #[arg(
            long,
            help = "Wait until the configuration is valid and the host has joined an oracle"
        )]
        wait: bool,
    },
    Check {
        #[arg(long, help = "Also test Docker in a test instance")]
        docker: bool,
    },
    Trust {
        token: String,
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        #[arg(
            long,
            help = "Use the system CA, and do not pin the server certificate"
        )]
        no_pin: bool,
        #[arg(long)]
        force: bool,
    },
    Setup,
    #[command(
        about = "Connect stdin and stdout to the agent of a codespace, for the SSH connection type"
    )]
    Stdio {
        #[arg(long)]
        codespace: String,
    },
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
        Command::Check { docker } => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            return runtime()?.block_on(check(config, dirs, docker));
        }
        Command::Trust {
            token,
            url,
            no_pin,
            force,
        } => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            runtime()?.block_on(trust(&config, &dirs, &token, url, no_pin, force))?;
        }
        Command::Setup => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            runtime()?.block_on(setup(&config, &dirs))?;
        }
        Command::Serve { wait } => {
            let runtime = runtime()?;
            if wait && !runtime.block_on(wait_until_ready(&path, &dirs)) {
                return Ok(ExitCode::SUCCESS);
            }
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(config.logging.level);
            runtime.block_on(serve(config, dirs))?;
        }
        Command::Stdio { codespace } => {
            let config = HostConfig::load(&path, &dirs)?;
            init_logging(LogLevel::Warn);
            return runtime()?.block_on(stdio(config, dirs, &codespace));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn not_ready(path: &Path, dirs: &Dirs) -> Option<String> {
    let config = match HostConfig::load(path, dirs) {
        Ok(config) => config,
        Err(err) => return Some(format!("{err}")),
    };
    if let Err(err) = JoinedOracle::load(&config.data_dir(dirs)) {
        return Some(format!("{err:#}"));
    }
    if let Err(err) = loom_host::driver(&config, dirs) {
        return Some(format!("{err:#}"));
    }
    None
}

async fn wait_until_ready(path: &Path, dirs: &Dirs) -> bool {
    let mut last = None;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    loop {
        let Some(reason) = not_ready(path, dirs) else {
            return true;
        };
        if last.as_ref() != Some(&reason) {
            eprintln!("loom-host: waiting: {reason}");
            eprintln!(
                "loom-host: the configuration file is {}; the commands `trust`, `join`, and `setup` prepare the host",
                path.display()
            );
            last = Some(reason);
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = tokio::signal::ctrl_c() => return false,
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => return false,
        }
    }
}

async fn check(config: HostConfig, dirs: Dirs, docker: bool) -> anyhow::Result<ExitCode> {
    let data_dir = config.data_dir(&dirs);
    let mut failed = false;
    match JoinedOracle::load(&data_dir) {
        Ok(oracle) => println!("ok\toracle\t{} as {}", oracle.url, oracle.name),
        Err(err) => {
            failed = true;
            println!("failed\toracle\t{err:#}");
        }
    }
    let agents = loom_host::agents(&config, &dirs);
    match agents.fixed() {
        Some(path) if path.is_file() => println!("ok\tagent\t{}", path.display()),
        Some(path) => {
            failed = true;
            println!("failed\tagent\t{} is not there", path.display());
        }
        None => println!(
            "ok\tagent\tthe releases, by the version of each client, in {}",
            agents.dir().display()
        ),
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
    for item in driver.check(docker).await {
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

fn write_file(path: &Path, content: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    file.write_all(content)?;
    Ok(())
}

async fn trust(
    config: &HostConfig,
    dirs: &Dirs,
    token: &str,
    url: Option<String>,
    no_pin: bool,
    force: bool,
) -> anyhow::Result<()> {
    let parsed = TrustToken::parse(token)?;
    let url = match url.or_else(|| config.incus.url.clone()) {
        Some(url) => url,
        None => format!(
            "https://{}",
            parsed
                .addresses
                .first()
                .context("the token has no address; pass --url")?
        ),
    };
    let certificates = config.incus_certificates(dirs);
    if certificates.client_certificate.exists() && !force {
        bail!(
            "{} exists; pass --force to make a new certificate",
            certificates.client_certificate.display()
        );
    }
    let server = loom_incus::server_certificate(&url).await?;
    let seen = loom_incus::fingerprint(&server);
    if !seen.eq_ignore_ascii_case(&parsed.fingerprint) {
        bail!(
            "the server certificate of {url} is {seen}, but the token names {}",
            parsed.fingerprint
        );
    }
    for path in [
        &certificates.client_certificate,
        &certificates.client_key,
        &certificates.server_certificate,
    ] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot make {}", parent.display()))?;
        }
    }
    let name = if parsed.client_name.is_empty() {
        "loom-host".to_owned()
    } else {
        parsed.client_name.clone()
    };
    let generated = loom_incus::generate_certificate(&name)?;
    write_file(&certificates.client_key, generated.key.as_bytes(), 0o600)?;
    write_file(
        &certificates.client_certificate,
        generated.certificate.as_bytes(),
        0o644,
    )?;
    let pinned = (!no_pin).then_some(certificates.server_certificate.as_path());
    if pinned.is_some() {
        write_file(
            &certificates.server_certificate,
            loom_incus::certificate_pem(&server).as_bytes(),
            0o644,
        )?;
    } else if certificates.server_certificate.exists() {
        std::fs::remove_file(&certificates.server_certificate)?;
    }
    let incus = loom_incus::load_https(
        &url,
        &certificates.client_certificate,
        &certificates.client_key,
        pinned,
    )?;
    incus.add_certificate(token).await?;
    let server = incus.server().await?;
    if server.auth != "trusted" {
        bail!("Incus took the token, but it does not trust the new certificate yet");
    }
    println!(
        "Incus {} at {url} trusts this host now.",
        server.environment.server_version
    );
    if config.incus.url.as_deref() != Some(url.as_str()) || config.driver != HostDriver::Incus {
        println!(
            "Set these keys in host.yaml:\n\ndriver: incus\nincus:\n  url: {url}\n  project: <project>\n  pool: <pool>\n  network: <bridge>"
        );
    }
    Ok(())
}

async fn setup(config: &HostConfig, dirs: &Dirs) -> anyhow::Result<()> {
    let driver = loom_host::incus_driver(config, dirs)?;
    if !driver.incus().is_local() {
        bail!(
            "`loom-host setup` needs the local Incus socket; with a certificate, make the project with the incus CLI"
        );
    }
    let project = &config.incus.project;
    match driver.setup_project().await? {
        SetupResult::Created => println!("made the restricted Incus project {project}"),
        SetupResult::Updated(keys) => {
            println!("set {} on the Incus project {project}", keys.join(", "))
        }
        SetupResult::Unchanged => {
            println!("the Incus project {project} exists already")
        }
    }
    Ok(())
}

async fn stdio(config: HostConfig, dirs: Dirs, codespace: &str) -> anyhow::Result<ExitCode> {
    let oracle = JoinedOracle::load(&config.data_dir(&dirs))?;
    let driver = loom_host::driver(&config, &dirs)?;
    let code = loom_host::stdio(
        driver.as_ref(),
        &oracle.host_id,
        codespace,
        Box::new(tokio::io::stdin()),
        Box::new(tokio::io::stdout()),
        Box::new(tokio::io::stderr()),
    )
    .await?;
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
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
