use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use loom_client::oracle_proto::{
    Codespace, CodespaceState, Device, DockerSettings, Environment, Event, GitCredential,
    GitCredentialInput, GitProvider, GitProviderInput, Host, Pairing, PairingLink, Preset,
    ProviderRoute, RelayHost, Repository, Secret, Workspace,
};
use loom_client::{
    AccessToken, ClientIdentity, NewCodespace, OracleClient, OracleEndpoint, OracleEvents,
    pair_oracle,
};
use loom_config::{ByteSize, Dirs, HumanDuration};
use serde::{Deserialize, Serialize};

const STATE_ENV: &str = "LOOM_ORACLE_CLI_STATE";
const IDENTITY_FILE: &str = "identity.json";
const ORACLE_FILE: &str = "oracle.json";
const CLIENT_NAME: &str = "loom-oracle-cli";

#[derive(Parser)]
#[command(
    name = "loom-oracle-cli",
    version = loom_version::VERSION,
    about = "Test client for the control service of a Loom-Next oracle"
)]
struct Cli {
    #[arg(long, global = true, env = STATE_ENV)]
    state_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Pair {
        link: String,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, value_name = "PATH")]
        identity: Option<PathBuf>,
        #[arg(long, requires = "access_secret_env")]
        access_id_env: Option<String>,
        #[arg(long, requires = "access_id_env")]
        access_secret_env: Option<String>,
    },
    Info,
    Devices,
    RenameDevice {
        id: u64,
        label: String,
    },
    RevokeDevice {
        id: u64,
    },
    PushToken {
        #[arg(required_unless_present = "clear")]
        token: Option<String>,
        #[arg(long, conflicts_with = "token")]
        clear: bool,
    },
    PairDevice {
        #[arg(long, value_parser = parse_duration)]
        ttl: Option<Duration>,
        #[arg(long)]
        no_qr: bool,
    },
    Pairings,
    CancelPairing {
        id: String,
    },
    RelayHosts,
    AddRelayHost {
        host_id: String,
        label: String,
    },
    RemoveRelayHost {
        host_id: String,
    },
    PairHost {
        #[arg(long, value_parser = parse_duration)]
        ttl: Option<Duration>,
        #[arg(long)]
        no_qr: bool,
    },
    Hosts,
    RemoveHost {
        name: String,
    },
    SetEnvironment {
        host: String,
        #[arg(long)]
        base_image: Option<String>,
        #[arg(long, value_name = "PATH")]
        setup_script: Option<PathBuf>,
        #[arg(long = "env", value_name = "NAME=VALUE", value_parser = parse_variable)]
        variables: Vec<(String, String)>,
        #[arg(long = "ca-certificate", value_name = "PATH")]
        ca_certificates: Vec<PathBuf>,
        #[arg(long = "registry-mirror", value_name = "URL")]
        registry_mirrors: Vec<String>,
    },
    Workspaces,
    CreateWorkspace {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long = "repository", value_name = "URL[,directory=DIR][,branch=BRANCH]", value_parser = parse_repository, required = true)]
        repositories: Vec<Repository>,
    },
    UpdateWorkspace {
        name: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long = "repository", value_name = "URL[,directory=DIR][,branch=BRANCH]", value_parser = parse_repository)]
        repositories: Vec<Repository>,
    },
    DeleteWorkspace {
        name: String,
    },
    Codespaces {
        #[arg(long)]
        all: bool,
    },
    CreateCodespace {
        workspace: String,
        #[arg(long)]
        preset: Option<String>,
        #[arg(long)]
        cpus: Option<u32>,
        #[arg(long, value_parser = parse_memory, value_name = "SIZE")]
        memory: Option<u64>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long = "base", value_name = "DIRECTORY=BRANCH", value_parser = parse_variable)]
        bases: Vec<(String, String)>,
        #[arg(long)]
        no_wait: bool,
    },
    CloseCodespace {
        id: String,
        #[arg(long)]
        no_wait: bool,
    },
    Providers,
    SetProvider {
        name: String,
        #[arg(long, value_name = "URL")]
        url: String,
        #[arg(long, value_name = "URL")]
        api_url: Option<String>,
        #[arg(long)]
        git_name: Option<String>,
        #[arg(long)]
        git_email: Option<String>,
        #[arg(long, value_parser = ["direct", "hosts"], default_value = "direct")]
        route: String,
    },
    DeleteProvider {
        name: String,
    },
    Credentials,
    SetCredential {
        name: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, requires = "owner")]
        repository: Option<String>,
        #[arg(
            long,
            value_name = "PATH",
            help = "Read the token from this file; '-' reads standard input"
        )]
        token_file: Option<PathBuf>,
    },
    CheckCredential {
        name: String,
    },
    DeleteCredential {
        name: String,
    },
    Presets,
    SetPreset {
        name: String,
        #[arg(value_name = "PATH")]
        file: PathBuf,
    },
    DeletePreset {
        name: String,
    },
    Search {
        provider: String,
        #[arg(default_value = "")]
        query: String,
    },
    Branches {
        provider: String,
        #[arg(value_name = "OWNER/REPOSITORY")]
        repository: String,
    },
    ClientEntry {
        id: String,
        #[arg(long)]
        name: Option<String>,
    },
    Watch {
        #[arg(long)]
        count: Option<usize>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedOracle {
    url: String,
    host_id: String,
    host_key: String,
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_id_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_secret_env: Option<String>,
}

impl SavedOracle {
    fn access(&self) -> anyhow::Result<Option<AccessToken>> {
        match (&self.access_id_env, &self.access_secret_env) {
            (Some(id), Some(secret)) => AccessToken::from_env(id, secret)
                .map(Some)
                .map_err(anyhow::Error::msg),
            _ => Ok(None),
        }
    }

    fn endpoint(&self) -> anyhow::Result<OracleEndpoint> {
        Ok(OracleEndpoint::from_parts(
            &self.url,
            &self.host_id,
            &self.host_key,
            self.access()?,
        )?)
    }
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<HumanDuration>()
        .map(|d| d.as_duration())
        .map_err(|e| e.to_string())
}

fn parse_memory(text: &str) -> Result<u64, String> {
    let size: ByteSize = text.parse()?;
    let mib = size.as_u64() / (1 << 20);
    if mib == 0 {
        return Err(format!(
            "{text:?} is less than 1 MiB; use a unit such as 4GiB"
        ));
    }
    Ok(mib)
}

fn parse_variable(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not NAME=VALUE"))?;
    Ok((name.to_owned(), value.to_owned()))
}

fn parse_repository(text: &str) -> Result<Repository, String> {
    let mut parts = text.split(',');
    let mut repository = Repository {
        url: parts.next().unwrap_or_default().to_owned(),
        provider: None,
        directory: None,
        base_branch: None,
    };
    for part in parts {
        match part.split_once('=') {
            Some(("directory", value)) => repository.directory = Some(value.to_owned()),
            Some(("branch", value)) => repository.base_branch = Some(value.to_owned()),
            Some(("provider", value)) => repository.provider = Some(value.to_owned()),
            _ => {
                return Err(format!(
                    "{part:?} is not directory=DIR, branch=BRANCH, or provider=NAME"
                ));
            }
        }
    }
    Ok(repository)
}

fn read_token(path: &Path) -> anyhow::Result<Secret> {
    let mut text = String::new();
    if path == Path::new("-") {
        std::io::stdin()
            .read_to_string(&mut text)
            .context("cannot read the token from standard input")?;
    } else {
        text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the token from {}", path.display()))?;
    }
    let token = text.trim();
    if token.is_empty() {
        bail!("the token is empty");
    }
    Ok(Secret::new(token))
}

fn default_label() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| loom_crypto::validate_label(h).is_ok())
        .unwrap_or_else(|| CLIENT_NAME.to_owned())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("loom-oracle-cli: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("loom-oracle-cli: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn state_dir(cli: &Cli) -> anyhow::Result<PathBuf> {
    let dirs = Dirs::from_env()?;
    Ok(match &cli.state_dir {
        Some(dir) => dirs.expand(dir),
        None => dirs.default_data_dir().join("oracle-cli"),
    })
}

fn load_saved(dir: &Path) -> anyhow::Result<SavedOracle> {
    let path = dir.join(ORACLE_FILE);
    let text = match std::fs::read(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            bail!("no oracle is paired; run `loom-oracle-cli pair <link>` first")
        }
        Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
    };
    serde_json::from_slice(&text).with_context(|| format!("cannot parse {}", path.display()))
}

fn identity(dir: &Path, file: Option<&Path>) -> anyhow::Result<ClientIdentity> {
    let path = match file {
        Some(file) => file.to_path_buf(),
        None => dir.join(IDENTITY_FILE),
    };
    ClientIdentity::load_or_create(&path)
        .with_context(|| format!("cannot read the key {}", path.display()))
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn time(unix: u64) -> String {
    let elapsed = now_unix().saturating_sub(unix);
    format!("{} ago", HumanDuration(Duration::from_secs(elapsed)))
}

fn print_devices(devices: &[Device], me: u64) {
    for device in devices {
        let mut marks = Vec::new();
        if device.id == me {
            marks.push("this device".to_owned());
        }
        if device.connected {
            marks.push("connected".to_owned());
        } else if let Some(seen) = device.last_seen {
            marks.push(format!("seen {}", time(seen)));
        }
        if device.push {
            marks.push("push".to_owned());
        }
        println!("{}\t{}\t{}", device.id, device.label, marks.join(", "));
    }
}

fn print_pairing(pairing: &Pairing) {
    let left = pairing.expires_at.saturating_sub(now_unix());
    println!(
        "{}\t{}\texpires in {}",
        pairing.id,
        pairing.role,
        HumanDuration(Duration::from_secs(left))
    );
}

fn print_relay_host(host: &RelayHost) {
    let agents = if host.agents.is_empty() {
        "offline".to_owned()
    } else {
        host.agents.join(", ")
    };
    println!("{}\t{}\t{}", host.host_id, host.label, agents);
}

fn print_link(link: &PairingLink, qr: bool, what: &str) {
    if qr && let Ok(code) = qrcode::QrCode::new(link.link.as_bytes()) {
        let image = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build();
        println!("{image}");
    }
    println!("{}", link.link);
    let ttl = link
        .pairing
        .expires_at
        .saturating_sub(link.pairing.created_at);
    println!(
        "This link pairs one {what}. It works once and expires in {}.",
        HumanDuration(Duration::from_secs(ttl))
    );
}

fn print_host(host: &Host) {
    let mut marks = vec![if host.connected {
        "connected".to_owned()
    } else {
        "offline".to_owned()
    }];
    if let Some(report) = &host.report {
        marks.push(format!(
            "{} driver, {} CPUs, {}",
            report.driver,
            report.cpus,
            ByteSize::mib(report.memory_mib)
        ));
        if let Some(storage) = &report.storage_driver {
            marks.push(format!("storage {storage}"));
        }
    }
    marks.push(format!("{} codespaces", host.codespaces));
    for reach in &host.reach {
        marks.push(match &reach.error {
            None => format!("reaches {}", reach.name),
            Some(error) => format!("cannot reach {}: {error}", reach.name),
        });
    }
    println!("{}\t{}", host.name, marks.join(", "));
}

fn print_workspace(workspace: &Workspace) {
    let repositories: Vec<String> = workspace
        .repositories
        .iter()
        .map(|repository| {
            let directory = repository.directory.as_deref().unwrap_or("?");
            match &repository.base_branch {
                Some(branch) => format!("{directory}={} ({branch})", repository.url),
                None => format!("{directory}={}", repository.url),
            }
        })
        .collect();
    println!(
        "{}\t{}\t{}",
        workspace.name,
        workspace.host,
        repositories.join(", ")
    );
}

fn print_codespace(codespace: &Codespace) {
    let mut detail = format!(
        "{}, {} CPUs, {}",
        codespace.state,
        codespace.cpus,
        ByteSize::mib(codespace.memory_mib)
    );
    if let Some(step) = &codespace.step {
        detail.push_str(&format!(", step {step}"));
    }
    if let Some(error) = &codespace.error {
        detail.push_str(&format!(", {error}"));
    }
    if let Some(preset) = &codespace.preset {
        detail.push_str(&format!(", preset {preset}"));
    }
    println!(
        "{}\t{}\t{}\t{}\t{}",
        codespace.id, codespace.workspace, codespace.host, codespace.branch, detail
    );
}

fn print_provider(provider: &GitProvider) {
    let mut marks = vec![format!("API {}", provider.api_url)];
    if provider.route != ProviderRoute::Direct {
        marks.push(format!("route {}", provider.route));
    }
    if let Some(name) = &provider.git_name {
        marks.push(format!("Git name {name}"));
    }
    if let Some(email) = &provider.git_email {
        marks.push(format!("Git email {email}"));
    }
    println!(
        "{}\t{}\t{}",
        provider.name,
        provider.web_url,
        marks.join(", ")
    );
}

fn print_credential(credential: &GitCredential) {
    let mut marks = Vec::new();
    if let Some(login) = &credential.login {
        marks.push(format!("login {login}"));
    }
    if let Some(expires) = credential.expires_at {
        marks.push(format!("expires {}", time(expires)));
    }
    match (&credential.check_error, credential.checked_at) {
        (Some(error), _) => marks.push(format!("check failed: {error}")),
        (None, Some(at)) => marks.push(format!("checked {}", time(at))),
        (None, None) => marks.push("not checked".to_owned()),
    }
    println!(
        "{}\t{}\t{}\t{}",
        credential.name,
        credential.provider,
        credential.scope(),
        marks.join(", ")
    );
}

fn print_preset(preset: &Preset) {
    let credentials = if preset.git_credentials.is_empty() {
        "no Git credentials".to_owned()
    } else {
        format!("Git credentials {}", preset.git_credentials.join(", "))
    };
    println!(
        "{}\t{credentials}\tchanged {}",
        preset.name,
        time(preset.updated_at)
    );
}

fn print_event(event: &Event) {
    match event {
        Event::DeviceChanged { device } => println!(
            "device {} {}{}",
            device.id,
            device.label,
            if device.connected { " (connected)" } else { "" }
        ),
        Event::DeviceRemoved { id } => println!("device {id} removed"),
        Event::PairingChanged { pairing } => println!("pairing {} created", pairing.id),
        Event::PairingRemoved { id, used: true } => println!("pairing {id} used"),
        Event::PairingRemoved { id, used: false } => println!("pairing {id} removed"),
        Event::RelayHostChanged { relay_host } => {
            println!("relay host {} {}", relay_host.host_id, relay_host.label)
        }
        Event::RelayHostRemoved { host_id } => println!("relay host {host_id} removed"),
        Event::HostChanged { host } => {
            print!("host ");
            print_host(host);
        }
        Event::HostRemoved { name } => println!("host {name} removed"),
        Event::WorkspaceChanged { workspace } => {
            print!("workspace ");
            print_workspace(workspace);
        }
        Event::WorkspaceRemoved { name } => println!("workspace {name} removed"),
        Event::CodespaceChanged { codespace } => {
            print!("codespace ");
            print_codespace(codespace);
        }
        Event::CodespaceProgress { id, step } => println!("codespace {id} step {step}"),
        Event::CodespaceRemoved { id } => println!("codespace {id} removed"),
        Event::GitProviderChanged { provider } => {
            print!("provider ");
            print_provider(provider);
        }
        Event::GitProviderRemoved { name } => println!("provider {name} removed"),
        Event::GitCredentialChanged { credential } => {
            print!("credential ");
            print_credential(credential);
        }
        Event::GitCredentialRemoved { name } => println!("credential {name} removed"),
        Event::PresetChanged { preset } => {
            print!("preset ");
            print_preset(preset);
        }
        Event::PresetRemoved { name } => println!("preset {name} removed"),
        Event::DeviceKeysChanged => println!("the device keys changed"),
        Event::GitChanged => println!("the Git setup changed"),
        Event::EventsLost => println!("some events were lost"),
        Event::Unknown => println!("an unknown event"),
    }
}

fn client_entry(
    saved: &SavedOracle,
    codespace: &Codespace,
    name: Option<String>,
) -> anyhow::Result<String> {
    let Some(agent) = &codespace.agent else {
        bail!(
            "the agent of the codespace {} has not reported its keys yet",
            codespace.id
        );
    };
    let name = name.unwrap_or_else(|| format!("{}-{}", codespace.workspace, codespace.id));
    let mut entry = format!(
        "- kind: relay\n  name: {name}\n  url: {}\n  host_id: {}\n  host_key: {}\n",
        saved.url, agent.host_id, agent.host_key
    );
    if let (Some(id), Some(secret)) = (&saved.access_id_env, &saved.access_secret_env) {
        entry.push_str(&format!(
            "  cloudflare_access:\n    client_id_env: {id}\n    client_secret_env: {secret}\n"
        ));
    }
    Ok(entry)
}

async fn wait_codespace(
    client: &OracleClient,
    events: &mut OracleEvents,
    id: &str,
    json: bool,
    done: impl Fn(CodespaceState) -> bool,
) -> anyhow::Result<Codespace> {
    let current = client.codespaces().await?;
    if let Some(codespace) = current.into_iter().find(|c| c.id == id)
        && done(codespace.state)
    {
        return Ok(codespace);
    }
    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => bail!("stopped waiting; the codespace {id} continues"),
            event = events.recv() => event,
        };
        let Some(event) = event else {
            bail!("the oracle closed the connection");
        };
        match &event {
            Event::CodespaceProgress { id: changed, step } if changed == id => {
                if json {
                    print_json(&event)?;
                } else {
                    println!("{step}");
                }
            }
            Event::CodespaceChanged { codespace } if codespace.id == id => {
                if done(codespace.state) {
                    return Ok(codespace.clone());
                }
                if matches!(
                    codespace.state,
                    CodespaceState::Failed | CodespaceState::Lost | CodespaceState::Closed
                ) {
                    bail!(
                        "the codespace {id} is {}: {}",
                        codespace.state,
                        codespace.error.as_deref().unwrap_or("no reason given")
                    );
                }
            }
            _ => {}
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let dir = state_dir(&cli)?;
    let json = cli.json;
    if let Command::Pair {
        link,
        label,
        identity: identity_file,
        access_id_env,
        access_secret_env,
    } = cli.command
    {
        let saved = SavedOracle {
            url: String::new(),
            host_id: String::new(),
            host_key: String::new(),
            label: label.unwrap_or_else(default_label),
            identity: identity_file.map(|path| std::path::absolute(&path).unwrap_or(path)),
            access_id_env,
            access_secret_env,
        };
        let identity = identity(&dir, saved.identity.as_deref())?;
        let endpoint = pair_oracle(&link, &identity, &saved.label, saved.access()?).await?;
        let saved = SavedOracle {
            url: endpoint.url.clone(),
            host_id: endpoint.host.to_string(),
            host_key: endpoint.host_key_text(),
            ..saved
        };
        let path = dir.join(ORACLE_FILE);
        std::fs::create_dir_all(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
        std::fs::write(&path, serde_json::to_vec_pretty(&saved)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        if json {
            return print_json(&saved);
        }
        println!("paired with the oracle {} as {}", saved.url, saved.label);
        return Ok(());
    }

    let saved = load_saved(&dir)?;
    let (client, mut events) = OracleClient::connect(
        &saved.endpoint()?,
        &identity(&dir, saved.identity.as_deref())?,
        CLIENT_NAME,
    )
    .await
    .with_context(|| format!("cannot connect to the oracle {}", saved.url))?;
    match cli.command {
        Command::Pair { .. } => unreachable!(),
        Command::Info => {
            let info = client.info().await?;
            if json {
                return print_json(&serde_json::json!({
                    "oracle": info,
                    "peer": client.hello().peer,
                }));
            }
            let peer = &client.hello().peer;
            println!("oracle {} (protocol {})", info.version, info.protocol);
            println!("url {}", info.url);
            println!("host {}", info.host_id);
            println!("peer {} {} ({})", peer.id, peer.label, peer.role);
        }
        Command::Devices => {
            let devices = client.devices().await?;
            if json {
                return print_json(&devices);
            }
            print_devices(&devices, client.hello().peer.id);
        }
        Command::RenameDevice { id, label } => {
            let device = client.rename_device(id, &label).await?;
            if json {
                return print_json(&device);
            }
            println!("device {} is now {}", device.id, device.label);
        }
        Command::RevokeDevice { id } => {
            client.revoke_device(id).await?;
            if !json {
                println!("revoked device {id}");
            }
        }
        Command::PushToken { token, clear: _ } => {
            client.set_push_token(token.as_deref()).await?;
            if !json {
                match token {
                    Some(_) => println!("set the push token"),
                    None => println!("cleared the push token"),
                }
            }
        }
        Command::PairDevice { ttl, no_qr } => {
            let link = client.create_device_pairing(ttl).await?;
            if json {
                return print_json(&link);
            }
            print_link(&link, !no_qr, "device");
        }
        Command::Pairings => {
            let pairings = client.pairings().await?;
            if json {
                return print_json(&pairings);
            }
            pairings.iter().for_each(print_pairing);
        }
        Command::CancelPairing { id } => {
            client.cancel_pairing(&id).await?;
            if !json {
                println!("cancelled pairing {id}");
            }
        }
        Command::RelayHosts => {
            let hosts = client.relay_hosts().await?;
            if json {
                return print_json(&hosts);
            }
            hosts.iter().for_each(print_relay_host);
        }
        Command::AddRelayHost { host_id, label } => {
            let host = client.add_relay_host(&host_id, &label).await?;
            if json {
                return print_json(&host);
            }
            print_relay_host(&host);
        }
        Command::RemoveRelayHost { host_id } => {
            client.remove_relay_host(&host_id).await?;
            if !json {
                println!("removed relay host {host_id}");
            }
        }
        Command::PairHost { ttl, no_qr } => {
            let link = client.create_host_pairing(ttl).await?;
            if json {
                return print_json(&link);
            }
            print_link(&link, !no_qr, "host");
            println!("On the host, run: loom-host join '{}'", link.link);
        }
        Command::Hosts => {
            let hosts = client.hosts().await?;
            if json {
                return print_json(&hosts);
            }
            hosts.iter().for_each(print_host);
        }
        Command::RemoveHost { name } => {
            client.remove_host(&name).await?;
            if !json {
                println!("removed host {name}");
            }
        }
        Command::SetEnvironment {
            host,
            base_image,
            setup_script,
            variables,
            ca_certificates,
            registry_mirrors,
        } => {
            let read = |path: &Path| {
                std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))
            };
            let environment = Environment {
                base_image,
                setup_script: setup_script.as_deref().map(read).transpose()?,
                variables: variables.into_iter().collect(),
                ca_certificates: ca_certificates
                    .iter()
                    .map(|path| read(path))
                    .collect::<anyhow::Result<_>>()?,
                docker: DockerSettings { registry_mirrors },
            };
            let host = client.set_host_environment(&host, environment).await?;
            if json {
                return print_json(&host);
            }
            println!("set the environment of the host {}", host.name);
        }
        Command::Workspaces => {
            let workspaces = client.workspaces().await?;
            if json {
                return print_json(&workspaces);
            }
            workspaces.iter().for_each(print_workspace);
        }
        Command::CreateWorkspace {
            name,
            host,
            repositories,
        } => {
            let workspace = client.create_workspace(&name, &host, repositories).await?;
            if json {
                return print_json(&workspace);
            }
            print_workspace(&workspace);
        }
        Command::UpdateWorkspace {
            name,
            host,
            repositories,
        } => {
            let repositories = (!repositories.is_empty()).then_some(repositories);
            let workspace = client
                .update_workspace(&name, host.as_deref(), repositories)
                .await?;
            if json {
                return print_json(&workspace);
            }
            print_workspace(&workspace);
        }
        Command::DeleteWorkspace { name } => {
            client.delete_workspace(&name).await?;
            if !json {
                println!("deleted workspace {name}");
            }
        }
        Command::Codespaces { all } => {
            let codespaces: Vec<Codespace> = client
                .codespaces()
                .await?
                .into_iter()
                .filter(|c| all || c.state != CodespaceState::Closed)
                .collect();
            if json {
                return print_json(&codespaces);
            }
            codespaces.iter().for_each(print_codespace);
        }
        Command::CreateCodespace {
            workspace,
            preset,
            cpus,
            memory,
            branch,
            bases,
            no_wait,
        } => {
            if !no_wait {
                client.subscribe().await?;
            }
            let created = client
                .create_codespace_with_warnings(NewCodespace {
                    workspace,
                    preset,
                    cpus,
                    memory_mib: memory,
                    base_branch: branch,
                    base_branches: bases.into_iter().collect::<BTreeMap<_, _>>(),
                    client_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                })
                .await?;
            for warning in &created.warnings {
                eprintln!("warning: {warning}");
            }
            let codespace = created.codespace;
            let codespace = if no_wait {
                codespace
            } else {
                if !json {
                    println!("creating the codespace {}", codespace.id);
                }
                wait_codespace(&client, &mut events, &codespace.id, json, |state| {
                    state == CodespaceState::Ready
                })
                .await?
            };
            if json {
                return print_json(&codespace);
            }
            print_codespace(&codespace);
            if codespace.state == CodespaceState::Ready {
                println!("Add this host entry to client.yaml to connect with the desktop client:");
                print!("{}", client_entry(&saved, &codespace, None)?);
            }
        }
        Command::CloseCodespace { id, no_wait } => {
            if !no_wait {
                client.subscribe().await?;
            }
            let mut codespace = client.close_codespace(&id).await?;
            if !no_wait {
                codespace = wait_codespace(&client, &mut events, &id, json, |state| {
                    state == CodespaceState::Closed
                })
                .await?;
            }
            if json {
                return print_json(&codespace);
            }
            print_codespace(&codespace);
        }
        Command::Providers => {
            let providers = client.git_providers().await?;
            if json {
                return print_json(&providers);
            }
            providers.iter().for_each(print_provider);
        }
        Command::SetProvider {
            name,
            url,
            api_url,
            git_name,
            git_email,
            route,
        } => {
            let provider = client
                .set_git_provider(GitProviderInput {
                    name,
                    web_url: url,
                    api_url,
                    git_name,
                    git_email,
                    route: ProviderRoute::parse(&route),
                })
                .await?;
            if json {
                return print_json(&provider);
            }
            print_provider(&provider);
        }
        Command::DeleteProvider { name } => {
            client.delete_git_provider(&name).await?;
            if !json {
                println!("deleted provider {name}");
            }
        }
        Command::Credentials => {
            let credentials = client.git_credentials().await?;
            if json {
                return print_json(&credentials);
            }
            credentials.iter().for_each(print_credential);
        }
        Command::SetCredential {
            name,
            provider,
            owner,
            repository,
            token_file,
        } => {
            let token = token_file.as_deref().map(read_token).transpose()?;
            let credential = client
                .set_git_credential(GitCredentialInput {
                    name,
                    provider,
                    owner,
                    repository,
                    token,
                })
                .await?;
            if json {
                return print_json(&credential);
            }
            print_credential(&credential);
        }
        Command::CheckCredential { name } => {
            let credential = client.check_git_credential(&name).await?;
            if json {
                return print_json(&credential);
            }
            print_credential(&credential);
        }
        Command::DeleteCredential { name } => {
            client.delete_git_credential(&name).await?;
            if !json {
                println!("deleted credential {name}");
            }
        }
        Command::Presets => {
            let presets = client.presets().await?;
            if json {
                return print_json(&presets);
            }
            presets.iter().for_each(print_preset);
        }
        Command::SetPreset { name, file } => {
            let yaml = std::fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let preset = client.set_preset(&name, &yaml).await?;
            if json {
                return print_json(&preset);
            }
            print_preset(&preset);
        }
        Command::DeletePreset { name } => {
            client.delete_preset(&name).await?;
            if !json {
                println!("deleted preset {name}");
            }
        }
        Command::Search { provider, query } => {
            let found = client.search_repositories(&provider, &query).await?;
            if json {
                return print_json(&found);
            }
            for error in &found.errors {
                eprintln!("warning: {error}");
            }
            for repository in &found.repositories {
                println!(
                    "{}/{}\t{}\t{}{}",
                    repository.owner,
                    repository.name,
                    repository.url,
                    if repository.private {
                        "private"
                    } else {
                        "public"
                    },
                    repository
                        .description
                        .as_deref()
                        .map(|d| format!(", {d}"))
                        .unwrap_or_default()
                );
            }
            if found.more {
                println!("(more results; refine the query)");
            }
        }
        Command::Branches {
            provider,
            repository,
        } => {
            let (owner, name) = repository
                .split_once('/')
                .context("give the repository as OWNER/REPOSITORY")?;
            let branches = client.branches(&provider, owner, name).await?;
            if json {
                return print_json(&branches);
            }
            for branch in &branches.branches {
                let mark = if branches.default_branch.as_ref() == Some(branch) {
                    "\tdefault"
                } else {
                    ""
                };
                println!("{branch}{mark}");
            }
            if branches.more {
                println!("(more branches)");
            }
        }
        Command::ClientEntry { id, name } => {
            let codespace = client
                .codespaces()
                .await?
                .into_iter()
                .find(|c| c.id == id)
                .with_context(|| format!("no codespace has the ID {id}"))?;
            print!("{}", client_entry(&saved, &codespace, name)?);
        }
        Command::Watch { count } => {
            client.subscribe().await?;
            if json {
                print_json(&serde_json::json!({"type": "subscribed"}))?;
            } else {
                println!("watching the events of {}", saved.url);
            }
            let mut seen = 0;
            while count.is_none_or(|count| seen < count) {
                let event = tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    event = events.recv() => event,
                };
                let Some(event) = event else {
                    bail!("the oracle closed the connection");
                };
                if json {
                    print_json(&event)?;
                } else {
                    print_event(&event);
                }
                seen += 1;
            }
        }
    }
    Ok(())
}
