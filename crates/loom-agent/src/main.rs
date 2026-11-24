use std::io::Write;
use std::path::PathBuf;

use anyhow::bail;
use clap::{Args, Parser, Subcommand};
use loom_agent::logging::{init as init_logging, level_name};
use loom_agent::managed::{ManagedOptions, serve_managed};
use loom_agent::stdio::{bridge, find_target};
use loom_agent_core::data_dir;
use loom_config::{AgentConfig, CONFIG_ENV, Dirs, telemetry_from_claude_code};
use loom_llm::{Auth, CredentialStore, ProviderId, ProviderSet, http_client};

#[derive(Parser)]
#[command(name = "loom-agent", version = loom_version::VERSION, about = "Loom agent daemon")]
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
    #[command(subcommand)]
    Config(ConfigCommand),
    #[command(
        hide = true,
        about = "Gives Git the tokens of the oracle in a codespace"
    )]
    GitCredential {
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
        #[arg(value_name = "OPERATION")]
        operation: String,
    },
}

#[derive(Args)]
struct ServeArgs {
    #[arg(
        long,
        required = true,
        help = "Run as the agent of a codespace that the oracle manages"
    )]
    managed: bool,
    #[arg(long, requires = "managed", value_name = "PATH")]
    bootstrap: Option<PathBuf>,
}

#[derive(Args)]
struct StdioArgs {
    #[arg(
        long,
        required = true,
        help = "Connect to the agent of this codespace, as `loom-host stdio` does"
    )]
    managed: bool,
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
    /// Print a `telemetry` section built from the OpenTelemetry settings
    /// your organization publishes for Claude Code. Uses the Anthropic login.
    FetchTelemetry {
        /// Print all of the organization's managed settings as JSON instead.
        #[arg(long)]
        raw: bool,
    },
}

struct Ctx {
    dirs: Dirs,
    config_path: PathBuf,
}

impl Ctx {
    fn load(&self) -> anyhow::Result<AgentConfig> {
        Ok(AgentConfig::load(&self.config_path, &self.dirs)?)
    }
}

fn sandbox_exec(args: &[std::ffi::OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let (policy, command) = match loom_tools::sandbox::parse_args(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("loom-agent: {err}");
            std::process::exit(2);
        }
    };
    if let Err(err) = loom_tools::sandbox::enforce(&policy) {
        eprintln!("loom-agent: cannot sandbox the command: {err}");
        std::process::exit(126);
    }
    let err = std::process::Command::new(&command[0])
        .args(&command[1..])
        .exec();
    eprintln!(
        "loom-agent: cannot run {}: {err}",
        command[0].to_string_lossy()
    );
    std::process::exit(127);
}

fn main() {
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if raw
        .get(1)
        .is_some_and(|a| a == loom_tools::sandbox::SANDBOX_SUBCOMMAND)
    {
        sandbox_exec(&raw[2..]);
    }
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
            let config = AgentConfig::read_preset(&ctx.config_path, &ctx.dirs).unwrap_or_default();
            init_logging(level_name(config.logging.level));
            serve_managed(ManagedOptions {
                bootstrap: args.bootstrap,
                config_path: ctx.config_path,
                config,
                dirs: ctx.dirs,
            })
            .await
        }
        CliCommand::Stdio(_) => {
            init_logging("warn");
            let config = ctx.load()?;
            let entry = find_target(&config, &ctx.dirs).await?;
            bridge(&entry).await?;
            Ok(0)
        }
        CliCommand::Config(ConfigCommand::FetchTelemetry { raw }) => {
            init_logging("warn");
            fetch_telemetry(&ctx, raw).await
        }
        CliCommand::Config(command) => config_command(&ctx, command),
        CliCommand::GitCredential { socket, operation } => {
            loom_agent::codespace_git::credential_helper(&socket, &operation).await?;
            Ok(0)
        }
    }
}

async fn fetch_telemetry(ctx: &Ctx, raw: bool) -> anyhow::Result<i32> {
    let config = ctx.load()?;
    let store = CredentialStore::in_dir(&data_dir(&config, &ctx.dirs));
    let auth = Auth::new(store, http_client());
    let anthropic = ProviderId::from("anthropic");
    let providers = ProviderSet::from_config(&config.llm, &auth)?;
    if !providers.contains(&anthropic) {
        bail!(
            "the anthropic provider is disabled in {}",
            ctx.config_path.display()
        );
    }
    let Some(settings) = providers.get(&anthropic)?.managed_settings().await? else {
        bail!("the anthropic provider has no managed settings");
    };
    if raw {
        println!("{}", serde_json::to_string_pretty(&settings)?);
        return Ok(0);
    }
    if settings.is_empty() {
        bail!("your organization publishes no Claude Code settings");
    }
    let Some(imported) = telemetry_from_claude_code(&settings)? else {
        bail!("your organization's Claude Code settings configure no telemetry");
    };
    for note in &imported.notes {
        eprintln!("note: {note}");
    }
    println!(
        "# From your organization's Claude Code settings. Replace the telemetry\n\
         # section of {} with this; the agent picks it up when it restarts.",
        ctx.config_path.display()
    );
    print!("{}", imported.to_yaml());
    Ok(0)
}

fn config_command(ctx: &Ctx, command: ConfigCommand) -> anyhow::Result<i32> {
    match command {
        ConfigCommand::FetchTelemetry { .. } => unreachable!("handled in run"),
        ConfigCommand::Path => {
            println!("{}", ctx.config_path.display());
        }
        ConfigCommand::Check => {
            // A missing file loads as the defaults, which is right for the
            // agent but would make a mistyped `--config` path look valid.
            if !ctx.config_path.exists() {
                bail!(
                    "{} does not exist; the agent uses the defaults \
                     (create the file with `loom-agent config init`)",
                    ctx.config_path.display()
                );
            }
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
