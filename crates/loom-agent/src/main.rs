use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use loom_agent::serve::{ServeOptions, serve};
use loom_agent::stdio::{StdioTarget, bridge, find_target};
use loom_agent::workspace::canonical_workspace;
use loom_agent_core::data_dir;
use loom_config::{AgentConfig, CONFIG_ENV, Dirs, LogLevel};
use loom_crypto::{AuthorizedClients, HostIdentity, PairingUri, Pairings, encode_key};
use loom_transport::{AgentEntry, Registry};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tracing_subscriber::EnvFilter;

const LOG_ENV: &str = "LOOM_LOG";
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
#[command(name = "loom-agent", version, about = "Loom agent daemon")]
struct Cli {
    #[arg(long, global = true, env = CONFIG_ENV, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Serve(ServeArgs),
    Stdio(StdioArgs),
    Ps {
        #[arg(long)]
        json: bool,
    },
    Stop(StopArgs),
    #[command(subcommand)]
    Config(ConfigCommand),
    Host {
        #[arg(long)]
        json: bool,
    },
    Pair(PairArgs),
    #[command(subcommand)]
    Clients(ClientsCommand),
}

#[derive(Args)]
struct PairArgs {
    #[arg(long, conflicts_with = "instance")]
    workspace: Option<PathBuf>,
    #[arg(long)]
    instance: Option<String>,
    #[arg(long, default_value = "10m", value_parser = parse_duration)]
    ttl: Duration,
    #[arg(long)]
    no_wait: bool,
    #[arg(long)]
    no_qr: bool,
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<loom_config::HumanDuration>()
        .map(|d| d.as_duration())
        .map_err(|e| e.to_string())
}

#[derive(Subcommand)]
enum ClientsCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Revoke {
        label: String,
    },
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, conflicts_with = "default")]
    workspace: Option<PathBuf>,
    #[arg(long)]
    default: bool,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct Target {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    instance: Option<String>,
}

#[derive(Args)]
struct StdioArgs {
    #[command(flatten)]
    target: Target,
    #[arg(long, requires = "workspace")]
    spawn: bool,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct StopArgs {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    instance: Option<String>,
    #[arg(long)]
    all: bool,
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

fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

struct Ctx {
    dirs: Dirs,
    config_path: PathBuf,
}

impl Ctx {
    fn load(&self) -> anyhow::Result<AgentConfig> {
        Ok(AgentConfig::load(&self.config_path, &self.dirs)?)
    }

    fn registry(&self, config: &AgentConfig) -> Registry {
        Registry::new(config.runtime_dir(&self.dirs))
    }
}

fn main() {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("loom-agent: {err}");
            std::process::exit(1);
        }
    };
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("loom-agent: {err:#}");
            1
        }
    };
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    let dirs = Dirs::from_env()?;
    let config_path = dirs.agent_config_path(cli.config.as_deref(), None);
    let ctx = Ctx { dirs, config_path };
    match cli.command {
        CliCommand::Serve(args) => {
            let config = ctx.load()?;
            init_logging(level_name(config.logging.level));
            let workspace = if args.default { None } else { args.workspace };
            if workspace.is_none() && !args.default {
                bail!("pass --workspace <path> or --default");
            }
            serve(ServeOptions {
                workspace,
                config_path: ctx.config_path,
                config,
                dirs: ctx.dirs,
            })
            .await
        }
        CliCommand::Stdio(args) => {
            init_logging("warn");
            let config = ctx.load()?;
            let target = match (args.target.workspace, args.target.instance) {
                (Some(path), _) => StdioTarget::Workspace {
                    path,
                    spawn: args.spawn,
                },
                (None, Some(id)) => StdioTarget::Instance(id),
                (None, None) => bail!("pass --workspace or --instance"),
            };
            let entry = find_target(&target, &ctx.config_path, &config, &ctx.dirs).await?;
            bridge(&entry).await?;
            Ok(0)
        }
        CliCommand::Ps { json } => {
            init_logging("warn");
            let config = ctx.load()?;
            let entries = ctx.registry(&config).list()?;
            print_agents(&entries, json)?;
            Ok(0)
        }
        CliCommand::Stop(args) => {
            init_logging("warn");
            let config = ctx.load()?;
            let registry = ctx.registry(&config);
            let entries = registry.list()?;
            let targets: Vec<AgentEntry> = if args.all {
                entries
            } else if let Some(id) = args.instance {
                let found: Vec<_> = entries.into_iter().filter(|e| e.instance == id).collect();
                if found.is_empty() {
                    bail!("no agent with instance {id} is running");
                }
                found
            } else if let Some(path) = args.workspace {
                let workspace = canonical_workspace(&path, &ctx.dirs)?;
                let found: Vec<_> = entries
                    .into_iter()
                    .filter(|e| e.workspace == workspace)
                    .collect();
                if found.is_empty() {
                    bail!("no agent is running for {}", workspace.display());
                }
                found
            } else {
                Vec::new()
            };
            stop_agents(&registry, &targets).await
        }
        CliCommand::Config(command) => config_command(&ctx, command),
        CliCommand::Host { json } => {
            init_logging("warn");
            let config = ctx.load()?;
            let identity = HostIdentity::load_or_create(&data_dir(&config, &ctx.dirs))?;
            let host = identity.host_id().to_string();
            let key = encode_key(&identity.static_public());
            if json {
                println!(
                    "{}",
                    serde_json::json!({"host_id": host, "static_key": key, "relay_url": config.relay.url})
                );
            } else {
                println!("host_id: {host}");
                println!("static_key: {key}");
            }
            Ok(0)
        }
        CliCommand::Pair(args) => {
            init_logging("warn");
            let config = ctx.load()?;
            pair(&ctx, &config, args).await
        }
        CliCommand::Clients(command) => {
            init_logging("warn");
            let config = ctx.load()?;
            let clients = AuthorizedClients::in_dir(&data_dir(&config, &ctx.dirs));
            match command {
                ClientsCommand::List { json } => {
                    let list = clients.list()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&list)?);
                    } else if list.is_empty() {
                        println!("no clients are authorized");
                    } else {
                        println!("{:<24}  {:<44}  ADDED", "LABEL", "KEY");
                        for client in list {
                            println!(
                                "{:<24}  {:<44}  {}",
                                client.label,
                                encode_key(&client.pubkey),
                                client.added_at
                            );
                        }
                    }
                    Ok(0)
                }
                ClientsCommand::Revoke { label } => {
                    if clients.revoke(&label)? {
                        println!("revoked {label}");
                        Ok(0)
                    } else {
                        bail!("no client is labeled {label}");
                    }
                }
            }
        }
    }
}

async fn pair(ctx: &Ctx, config: &AgentConfig, args: PairArgs) -> anyhow::Result<i32> {
    let Some(relay_url) = config.relay.url.clone().filter(|_| config.relay.enabled) else {
        bail!("the relay is not enabled; set relay.enabled and relay.url in agent.yaml");
    };
    let registry = ctx.registry(config);
    let entries = registry.list()?;
    let entry = if let Some(id) = &args.instance {
        entries.into_iter().find(|e| &e.instance == id)
    } else if let Some(path) = &args.workspace {
        let workspace = canonical_workspace(path, &ctx.dirs)?;
        entries.into_iter().find(|e| e.workspace == workspace)
    } else {
        entries.into_iter().next()
    };
    let Some(entry) = entry else {
        bail!("no matching agent is running; start one with `loom-agent serve`");
    };
    let data = data_dir(config, &ctx.dirs);
    let identity = HostIdentity::load_or_create(&data)?;
    let pairings = Pairings::in_dir(&data);
    let (id, secret) = pairings.create(args.ttl)?;
    let uri = PairingUri {
        relay_url,
        host: identity.host_id(),
        instance: loom_proto::InstanceId(entry.instance.clone()),
        host_static: identity.static_public(),
        secret,
    }
    .to_string();
    if !args.no_qr
        && let Ok(code) = qrcode::QrCode::new(uri.as_bytes())
    {
        let image = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{image}");
    }
    println!("{uri}");
    println!(
        "This code works once and expires in {}.",
        loom_config::HumanDuration(args.ttl)
    );
    if args.no_wait {
        return Ok(0);
    }
    let _ = std::io::stdout().flush();
    let deadline = tokio::time::Instant::now() + args.ttl;
    while tokio::time::Instant::now() < deadline {
        if !pairings.is_pending(&id)? {
            let clients = AuthorizedClients::in_dir(&data).list()?;
            match clients.iter().max_by_key(|c| c.added_at) {
                Some(client) => println!("paired with {}", client.label),
                None => println!("the pairing code was used"),
            }
            return Ok(0);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("the pairing code expired");
    Ok(1)
}

fn print_agents(entries: &[AgentEntry], json: bool) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    if json {
        serde_json::to_writer_pretty(&mut out, entries)?;
        writeln!(out)?;
        return Ok(());
    }
    if entries.is_empty() {
        writeln!(out, "no agents are running")?;
        return Ok(());
    }
    writeln!(out, "{:<32}  {:>8}  WORKSPACE", "INSTANCE", "PID")?;
    for entry in entries {
        writeln!(
            out,
            "{:<32}  {:>8}  {}",
            entry.instance,
            entry.pid,
            entry.workspace.display()
        )?;
    }
    Ok(())
}

async fn stop_agents(registry: &Registry, targets: &[AgentEntry]) -> anyhow::Result<i32> {
    let mut code = 0;
    for entry in targets {
        let pid = i32::try_from(entry.pid).context("invalid PID in the registry")?;
        if let Err(err) = kill(Pid::from_raw(pid), Signal::SIGTERM) {
            eprintln!(
                "could not signal {} (pid {}): {err}",
                entry.instance, entry.pid
            );
            code = 1;
            continue;
        }
        if registry
            .wait_until_gone(&entry.instance, STOP_TIMEOUT)
            .await?
        {
            println!("stopped {} ({})", entry.instance, entry.workspace.display());
        } else {
            eprintln!("{} did not stop within {:?}", entry.instance, STOP_TIMEOUT);
            code = 1;
        }
    }
    Ok(code)
}

fn config_command(ctx: &Ctx, command: ConfigCommand) -> anyhow::Result<i32> {
    match command {
        ConfigCommand::Path => {
            println!("{}", ctx.config_path.display());
        }
        ConfigCommand::Check => {
            ctx.load()?;
            println!("{}: ok", ctx.config_path.display());
        }
        ConfigCommand::Schema => {
            println!(
                "{}",
                serde_json::to_string_pretty(&loom_config::agent_config_schema())?
            );
        }
        ConfigCommand::Init { force } => {
            if ctx.config_path.exists() && !force {
                bail!(
                    "{} already exists; pass --force to overwrite it",
                    ctx.config_path.display()
                );
            }
            if let Some(parent) = ctx.config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&ctx.config_path, AgentConfig::default().to_yaml())?;
            println!("wrote {}", ctx.config_path.display());
        }
    }
    Ok(0)
}
