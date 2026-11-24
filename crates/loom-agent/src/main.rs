use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use loom_agent::logging::{init as init_logging, level_name};
use loom_agent::managed::{ManagedOptions, serve_managed};
use loom_agent::serve::{ServeOptions, Served, serve};
use loom_agent::stdio::{StdioTarget, bridge, find_target};
use loom_agent::workspace::canonical_workspace;
use loom_agent_core::data_dir;
use loom_config::{AgentConfig, CONFIG_ENV, Dirs, telemetry_from_claude_code};
use loom_crypto::{AuthorizedClients, HostIdentity, PairingUri, Pairings, encode_key};
use loom_llm::{Auth, CredentialStore, ProviderId, ProviderSet, http_client};
use loom_proto::{
    ClientHello, ClientMessage, Command, ConfigReload, PROTOCOL_VERSION, Payload, ServerMessage,
};
use loom_transport::{AgentEntry, Registry};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const STOP_TIMEOUT: Duration = Duration::from_secs(15);
const RELOAD_TIMEOUT: Duration = Duration::from_secs(30);

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
    Ps {
        #[arg(long)]
        json: bool,
    },
    Stop(SelectArgs),
    /// Asks running agents to read their configuration file again.
    Reload(SelectArgs),
    #[command(subcommand)]
    Config(ConfigCommand),
    Host {
        #[arg(long)]
        json: bool,
    },
    Pair(PairArgs),
    #[command(subcommand)]
    Clients(ClientsCommand),
    /// Updates loom-agent from the latest release, or another one, where it
    /// is installed.
    Update(UpdateArgs),
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
struct UpdateArgs {
    /// Install this release, such as 0.4.0, instead of the latest. Also
    /// installs an older release.
    #[arg(long = "version", value_name = "VERSION")]
    release: Option<loom_update::Version>,
    /// Only tell whether a newer release is out; exits with 0 when this is
    /// the latest and 1 when it is not.
    #[arg(long, conflicts_with = "release")]
    check: bool,
    /// Install the release also when it is the version that runs.
    #[arg(long)]
    force: bool,
    #[arg(
        long,
        conflicts_with = "check",
        help = "Restart the running agents of the replaced binary in place; they keep their sessions"
    )]
    restart: bool,
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
    #[arg(long, conflicts_with_all = ["default", "managed"])]
    workspace: Option<PathBuf>,
    #[arg(long, conflicts_with = "managed")]
    default: bool,
    #[arg(long, help = "Run as the agent of a codespace that the oracle manages")]
    managed: bool,
    #[arg(long, requires = "managed", value_name = "PATH")]
    bootstrap: Option<PathBuf>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct Target {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    instance: Option<String>,
    #[arg(
        long,
        help = "Connect to the agent of this codespace, as `loom-host stdio` does"
    )]
    managed: bool,
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
struct SelectArgs {
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

    fn registry(&self, config: &AgentConfig) -> Registry {
        Registry::new(config.runtime_dir(&self.dirs))
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

/// Runs `exe` in place of this process, with the same arguments, so that
/// the agent keeps its PID, as systemd expects. Returns only on failure.
fn restart(exe: &std::path::Path) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let _ = std::io::stdout().flush();
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    anyhow::anyhow!("could not start {} again: {err}", exe.display())
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
            if args.managed {
                let config =
                    AgentConfig::read_preset(&ctx.config_path, &ctx.dirs).unwrap_or_default();
                init_logging(level_name(config.logging.level));
                let served = serve_managed(ManagedOptions {
                    bootstrap: args.bootstrap,
                    config_path: ctx.config_path,
                    config,
                    dirs: ctx.dirs,
                })
                .await?;
                return match served {
                    Served::Exit(code) => Ok(code),
                    Served::Restart(exe) => Err(restart(&exe)),
                };
            }
            let config = ctx.load()?;
            init_logging(level_name(config.logging.level));
            let workspace = if args.default { None } else { args.workspace };
            if workspace.is_none() && !args.default {
                bail!("pass --workspace <path> or --default");
            }
            let served = serve(ServeOptions {
                workspace,
                config_path: ctx.config_path,
                config,
                dirs: ctx.dirs,
                ready: None,
                repositories: None,
                credentials: None,
                config_loader: None,
                repository_dirs: Vec::new(),
            })
            .await?;
            match served {
                Served::Exit(code) => Ok(code),
                Served::Restart(exe) => Err(restart(&exe)),
            }
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
                (None, None) if args.target.managed => StdioTarget::Managed,
                (None, None) => bail!("pass --workspace, --instance, or --managed"),
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
            let targets = select_agents(&registry, args, &ctx.dirs)?;
            stop_agents(&registry, &targets).await
        }
        CliCommand::Reload(args) => {
            init_logging("warn");
            let config = ctx.load()?;
            let registry = ctx.registry(&config);
            let targets = select_agents(&registry, args, &ctx.dirs)?;
            Ok(reload_agents(&targets).await)
        }
        CliCommand::Config(ConfigCommand::FetchTelemetry { raw }) => {
            init_logging("warn");
            fetch_telemetry(&ctx, raw).await
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
        CliCommand::GitCredential { socket, operation } => {
            loom_agent::codespace_git::credential_helper(&socket, &operation).await?;
            Ok(0)
        }
        CliCommand::Update(args) => {
            init_logging("warn");
            update(&ctx, args).await
        }
    }
}

async fn update(ctx: &Ctx, args: UpdateArgs) -> anyhow::Result<i32> {
    use loom_update::{Package, Updater, current_version, install_dir};
    let updater = Updater::new();
    let current = current_version();
    let release = updater
        .release(Package::Agent, args.release.as_ref())
        .await?;
    if args.check {
        if release.version > current {
            println!("loom-agent {} is out; this is {current}", release.version);
            println!("Run `loom-agent update` to install it.");
            return Ok(1);
        }
        println!("loom-agent {current} is the latest release");
        return Ok(0);
    }
    if args.release.is_none() && release.version < current {
        println!(
            "loom-agent {current} is newer than the latest release, {}",
            release.version
        );
        return Ok(0);
    }
    if release.version == current && !args.force {
        println!("loom-agent {current} is up to date");
        if args.restart {
            let path = std::env::current_exe()?;
            restart_agents(ctx, &path, loom_version::VERSION, false);
        }
        return Ok(0);
    }
    let dir = install_dir()?;
    eprintln!("Downloading {}", release.archive);
    let installed = updater
        .install(Package::Agent, &release, &dir, |_, _| {})
        .await?;
    for item in &installed {
        println!(
            "Replaced {} at {} with {}",
            item.old.as_deref().unwrap_or(item.binary),
            item.path.display(),
            item.new
        );
    }
    let version = release.version.to_string();
    match installed.iter().find(|item| item.binary == "loom-agent") {
        Some(item) if args.restart => restart_agents(ctx, &item.path, &version, true),
        _ => {
            let old = running_agents(ctx)
                .iter()
                .filter(|e| e.version != version)
                .count();
            if old > 0 {
                println!(
                    "{old} running agent(s) have another version. Restart them to use this one:"
                );
                println!("  loom-agent update --restart");
            }
        }
    }
    Ok(0)
}

fn running_agents(ctx: &Ctx) -> Vec<AgentEntry> {
    match ctx.load() {
        Ok(config) => ctx.registry(&config).list().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn restart_agents(ctx: &Ctx, path: &std::path::Path, version: &str, replaced: bool) {
    use loom_agent::supervisor::{RESTART_SIGNAL, exe_of, restarts_on_signal};
    let mut others = Vec::new();
    for entry in running_agents(ctx) {
        if !replaced && entry.version == version {
            continue;
        }
        let signalled = exe_of(entry.pid).as_deref() == Some(path)
            && restarts_on_signal(&entry.version)
            && i32::try_from(entry.pid)
                .is_ok_and(|pid| kill(Pid::from_raw(pid), RESTART_SIGNAL).is_ok());
        if signalled {
            println!(
                "Restarting the agent of {} ({})",
                entry.workspace.display(),
                entry.version
            );
        } else if entry.version != version {
            others.push(entry);
        }
    }
    if !others.is_empty() {
        println!(
            "{} running agent(s) have another version and cannot restart in place; stop them and start them again:",
            others.len()
        );
        for entry in &others {
            println!(
                "  {} ({}), instance {}",
                entry.workspace.display(),
                entry.version,
                entry.instance
            );
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

/// The running agents that `args` names; an error when it names none.
fn select_agents(
    registry: &Registry,
    args: SelectArgs,
    dirs: &Dirs,
) -> anyhow::Result<Vec<AgentEntry>> {
    let entries = registry.list()?;
    if args.all {
        return Ok(entries);
    }
    if let Some(id) = args.instance {
        let found: Vec<_> = entries.into_iter().filter(|e| e.instance == id).collect();
        if found.is_empty() {
            bail!("no agent with instance {id} is running");
        }
        return Ok(found);
    }
    if let Some(path) = args.workspace {
        let workspace = canonical_workspace(&path, dirs)?;
        let found: Vec<_> = entries
            .into_iter()
            .filter(|e| e.workspace == workspace)
            .collect();
        if found.is_empty() {
            bail!("no agent is running for {}", workspace.display());
        }
        return Ok(found);
    }
    Ok(Vec::new())
}

/// Asks each agent to reload its configuration and prints what it did.
/// Returns 1 when an agent could not be reached or rejected the file.
async fn reload_agents(targets: &[AgentEntry]) -> i32 {
    let mut code = 0;
    for entry in targets {
        let name = format!("{} ({})", entry.instance, entry.workspace.display());
        match reload_agent(entry).await {
            Ok(ConfigReload::Unchanged) => println!("{name}: unchanged"),
            Ok(ConfigReload::Applied { restart_required }) if restart_required.is_empty() => {
                println!("{name}: reloaded")
            }
            Ok(ConfigReload::Applied { restart_required }) => println!(
                "{name}: reloaded; restart it to apply {}",
                restart_required.join(", ")
            ),
            Err(err) => {
                eprintln!("{name}: {err}");
                code = 1;
            }
        }
    }
    code
}

async fn reload_agent(entry: &AgentEntry) -> anyhow::Result<ConfigReload> {
    tokio::time::timeout(RELOAD_TIMEOUT, ask_reload(entry))
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for the agent"))?
}

/// Says hello to the agent over its socket and sends it one
/// `ReloadConfig`, skipping the notices that come before the reply.
async fn ask_reload(entry: &AgentEntry) -> anyhow::Result<ConfigReload> {
    let mut stream = loom_transport::connect_unix(&entry.socket)
        .await
        .with_context(|| format!("could not connect to {}", entry.socket.display()))?;
    let hello = ClientMessage::Hello(ClientHello {
        version: PROTOCOL_VERSION,
        client: "loom-agent reload".into(),
    });
    stream.send(loom_proto::encode(&hello)?.into()).await?;
    let request = ClientMessage::Request {
        id: 1,
        command: Command::ReloadConfig,
    };
    let mut sent = false;
    loop {
        let Some(frame) = stream.recv().await? else {
            bail!("the agent closed the connection");
        };
        match loom_proto::decode::<ServerMessage>(&frame)? {
            ServerMessage::Hello(_) if !sent => {
                stream.send(loom_proto::encode(&request)?.into()).await?;
                sent = true;
            }
            ServerMessage::Rejected(err) => {
                bail!("the agent rejected the connection: {}", err.message)
            }
            ServerMessage::Response { id: 1, result } => {
                return match result {
                    Ok(Payload::ConfigReloaded(reload)) => Ok(reload),
                    Ok(other) => bail!("unexpected reply from the agent: {other:?}"),
                    Err(err) => bail!("{}", err.message),
                };
            }
            _ => {}
        }
    }
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
