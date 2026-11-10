use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use loom_agent::serve::{ServeOptions, serve};
use loom_agent::stdio::{StdioTarget, bridge, find_target};
use loom_agent::workspace::canonical_workspace;
use loom_config::{AgentConfig, CONFIG_ENV, Dirs, LogLevel};
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
    }
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
