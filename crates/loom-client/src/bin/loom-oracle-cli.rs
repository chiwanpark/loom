use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use loom_client::oracle_proto::{
    Codespace, CodespaceState, Device, DockerSettings, Environment, Event, FirstMessage,
    GitCredential, GitCredentialInput, GitProvider, GitProviderInput, Host, ModelAccount,
    ModelLoginInput, ModelLoginMethod, ModelLoginPrompt, Pairing, PairingLink, Preset, PresetFile,
    ProviderRoute, RelayHost, Repository, Secret, SecretInfo, Volume, VolumeInput, VolumeKind,
    Workspace, WorkspaceVolume,
};
use loom_client::{
    AccessToken, Client, ClientIdentity, CloseGuardError, NewCodespace, OracleClient,
    OracleEndpoint, OracleEvents, RelayHost as AgentRelay, choices_from_lists, close_plan,
    pair_oracle,
};
use loom_config::{ByteSize, Dirs, HumanDuration};
use loom_proto::{PushRepositories, RepositoryStatus};
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
    CheckHost {
        name: String,
        #[arg(long, help = "Also test Docker in a test instance")]
        docker: bool,
    },
    SetSsh {
        host: String,
        #[arg(help = "The SSH destination, such as user@host")]
        destination: Option<String>,
        #[arg(
            long,
            conflicts_with = "destination",
            help = "Remove the SSH destination"
        )]
        clear: bool,
    },
    HostVolumes {
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
        #[arg(long = "volume", value_name = "VOLUME:PATH[:ro]", value_parser = parse_volume)]
        volumes: Vec<WorkspaceVolume>,
    },
    UpdateWorkspace {
        name: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long = "repository", value_name = "URL[,directory=DIR][,branch=BRANCH]", value_parser = parse_repository)]
        repositories: Vec<Repository>,
        #[arg(long = "volume", value_name = "VOLUME:PATH[:ro]", value_parser = parse_volume)]
        volumes: Vec<WorkspaceVolume>,
        #[arg(
            long,
            conflicts_with = "volumes",
            help = "Remove all volumes from the workspace"
        )]
        no_volumes: bool,
    },
    DeleteWorkspace {
        name: String,
    },
    Volumes,
    CreateVolume {
        name: String,
        #[arg(long, value_parser = parse_memory, value_name = "SIZE", conflicts_with = "path")]
        size: Option<u64>,
        #[arg(
            long,
            value_name = "PATH",
            help = "Use this directory of the host instead of an Incus volume"
        )]
        path: Option<String>,
    },
    DeleteVolume {
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
        message: Option<String>,
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "message",
            help = "Read the first message from this file; '-' reads standard input"
        )]
        message_file: Option<PathBuf>,
        #[arg(long, value_name = "PROVIDER/MODEL")]
        model: Option<String>,
        #[arg(long)]
        no_wait: bool,
    },
    CloseCodespace {
        id: String,
        #[arg(long)]
        no_wait: bool,
        #[arg(
            long = "push",
            value_name = "DIRECTORY",
            help = "Push the work of this repository first; '*' selects all"
        )]
        push: Vec<String>,
        #[arg(
            long = "discard",
            value_name = "DIRECTORY",
            help = "Discard the work of this repository; '*' selects all"
        )]
        discard: Vec<String>,
        #[arg(
            long,
            help = "Commit the uncommitted changes of the pushed repositories with this message"
        )]
        message: Option<String>,
        #[arg(long, help = "Close the codespace when its agent cannot tell its work")]
        force: bool,
    },
    Repositories {
        id: String,
    },
    Push {
        id: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long = "directory", value_name = "DIRECTORY")]
        directories: Vec<String>,
    },
    PullRequest {
        id: String,
        #[arg(long)]
        title: String,
        #[arg(long, conflicts_with = "body_file", default_value = "")]
        body: String,
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        draft: bool,
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
        #[arg(long, value_name = "PATH", help = "Set AGENTS.md from this file")]
        agents_md: Option<PathBuf>,
        #[arg(
            long,
            value_name = "DIRECTORY",
            help = "Set the skills from the skill directories in this directory"
        )]
        skills: Option<PathBuf>,
    },
    ShowPreset {
        name: String,
    },
    DeletePreset {
        name: String,
    },
    Accounts,
    SignIn {
        account: String,
        #[arg(long)]
        provider: String,
        #[arg(long, help = "Use the device code instead of the browser")]
        device_code: bool,
    },
    SetApiKey {
        account: String,
        #[arg(long)]
        provider: String,
        #[arg(
            long,
            value_name = "PATH",
            default_value = "-",
            help = "Read the API key from this file; '-' reads standard input"
        )]
        key_file: PathBuf,
    },
    DeleteAccount {
        name: String,
    },
    Models {
        preset: String,
    },
    Secrets,
    SetSecret {
        name: String,
        #[arg(
            long,
            value_name = "PATH",
            default_value = "-",
            help = "Read the value from this file; '-' reads standard input"
        )]
        value_file: PathBuf,
    },
    DeleteSecret {
        name: String,
    },
    Notes {
        workspace: String,
    },
    SetNotes {
        workspace: String,
        #[arg(
            value_name = "PATH",
            help = "Read the notes from this file; '-' reads standard input"
        )]
        file: PathBuf,
    },
    Sessions {
        #[arg(long)]
        codespace: Option<String>,
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

fn parse_volume(text: &str) -> Result<WorkspaceVolume, String> {
    let mut parts = text.split(':');
    let (Some(volume), Some(path)) = (parts.next(), parts.next()) else {
        return Err(format!("{text:?} is not VOLUME:PATH or VOLUME:PATH:ro"));
    };
    let read_only = match parts.next() {
        None => false,
        Some("ro") => true,
        Some(other) => return Err(format!("{other:?} is not ro")),
    };
    if parts.next().is_some() || volume.is_empty() || path.is_empty() {
        return Err(format!("{text:?} is not VOLUME:PATH or VOLUME:PATH:ro"));
    }
    Ok(WorkspaceVolume {
        volume: volume.to_owned(),
        path: path.to_owned(),
        read_only,
    })
}

fn read_token(path: &Path) -> anyhow::Result<Secret> {
    let text = read_text(path, "the token")?;
    let token = text.trim();
    if token.is_empty() {
        bail!("the token is empty");
    }
    Ok(Secret::new(token))
}

fn read_text(path: &Path, what: &str) -> anyhow::Result<String> {
    let mut text = String::new();
    if path == Path::new("-") {
        std::io::stdin()
            .read_to_string(&mut text)
            .with_context(|| format!("cannot read {what} from standard input"))?;
    } else {
        text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {what} from {}", path.display()))?;
    }
    Ok(text)
}

fn read_skills(root: &Path) -> anyhow::Result<Vec<PresetFile>> {
    loom_client::read_skills(root).map_err(anyhow::Error::msg)
}

fn print_account(account: &ModelAccount) {
    let mut marks = vec![account.kind.to_string()];
    marks.extend(account.email.clone());
    marks.extend(
        account
            .organization
            .clone()
            .map(|o| format!("organization {o}")),
    );
    if !account.presets.is_empty() {
        marks.push(format!("presets {}", account.presets.join(", ")));
    }
    if let Some(error) = &account.error {
        marks.push(format!("error: {error}"));
    }
    println!(
        "{}\t{}\t{}",
        account.name,
        account.provider,
        marks.join(", ")
    );
}

fn print_secret(secret: &SecretInfo) {
    let presets = if secret.presets.is_empty() {
        "no presets".to_owned()
    } else {
        format!("presets {}", secret.presets.join(", "))
    };
    println!(
        "{}\t{presets}\tchanged {}",
        secret.name,
        time(secret.updated_at)
    );
}

async fn connect_codespace(
    saved: &SavedOracle,
    identity: &ClientIdentity,
    codespace: &Codespace,
) -> anyhow::Result<Client> {
    let agent = codespace
        .agent
        .as_ref()
        .with_context(|| format!("the codespace {} has no agent yet", codespace.id))?;
    let relay =
        AgentRelay::from_parts(&saved.url, &agent.host_id, &agent.host_key, saved.access()?)?;
    let instance = agent.instance.clone().map(loom_proto::InstanceId);
    let stream = relay
        .connect(instance.as_ref(), identity)
        .await
        .with_context(|| format!("cannot reach the agent of the codespace {}", codespace.id))?;
    Ok(Client::connect(stream, CLIENT_NAME).await?.0)
}

async fn find_codespace(client: &OracleClient, id: &str) -> anyhow::Result<Codespace> {
    client
        .codespaces()
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .with_context(|| format!("no codespace has the ID {id}"))
}

fn work_text(status: &RepositoryStatus) -> String {
    if let Some(error) = &status.error {
        return format!("unknown: {error}");
    }
    let mut parts = Vec::new();
    if status.changed_files > 0 {
        parts.push(format!("{} changed files", status.changed_files));
    }
    if status.unpushed > 0 {
        parts.push(format!("{} unpushed commits", status.unpushed));
    }
    if parts.is_empty() {
        "no unsaved work".to_owned()
    } else {
        parts.join(", ")
    }
}

fn print_repository(status: &RepositoryStatus) {
    let mut marks = vec![work_text(status)];
    if let Some(base) = &status.base_branch {
        marks.push(format!("{} ahead of {base}", status.ahead));
    }
    if status.pushed {
        marks.push("pushed".to_owned());
    }
    if let Some(url) = &status.pull_request {
        marks.push(format!("pull request {url}"));
    }
    println!(
        "{}\t{}\t{}\t{}",
        status.directory,
        status.branch,
        status.url,
        marks.join(", ")
    );
}

async fn guard_close(
    agent: &Client,
    statuses: Vec<RepositoryStatus>,
    push: &[String],
    discard: &[String],
    message: Option<String>,
) -> anyhow::Result<()> {
    let choose = choices_from_lists(&statuses, push, discard)?;
    let to_push = match close_plan(&statuses, choose, message.is_some()) {
        Ok(to_push) => to_push,
        Err(CloseGuardError::Undecided(undecided)) => {
            let lines: Vec<String> = undecided
                .iter()
                .map(|s| format!("{}: {}", s.directory, work_text(s)))
                .collect();
            bail!(
                "these repositories have unsaved work:\n  {}\nchoose --push <directory> or --discard <directory> for each ('*' selects all)",
                lines.join("\n  ")
            );
        }
        Err(err) => bail!("{err}; give --message to commit the changes"),
    };
    if to_push.is_empty() {
        return Ok(());
    }
    let pushed = agent
        .push_repositories(PushRepositories {
            commit_message: message,
            directories: to_push,
        })
        .await?;
    let failed: Vec<String> = pushed
        .iter()
        .filter(|s| s.error.is_some() || s.has_unsaved_work())
        .map(|s| format!("{}: {}", s.directory, work_text(s)))
        .collect();
    if !failed.is_empty() {
        bail!("cannot push:\n  {}", failed.join("\n  "));
    }
    for status in &pushed {
        eprintln!("pushed {} to {}", status.directory, status.branch);
    }
    Ok(())
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
    if let Some(ssh) = &host.ssh {
        marks.push(format!("ssh {ssh}"));
    }
    let problems = host
        .checks
        .iter()
        .filter(|c| c.status != loom_client::oracle_proto::HostCheckStatus::Ok)
        .count();
    if problems > 0 {
        marks.push(format!("{problems} check problems"));
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
    let volumes: Vec<String> = workspace
        .volumes
        .iter()
        .map(|mount| {
            let mode = if mount.read_only { ":ro" } else { "" };
            format!("{}:{}{mode}", mount.volume, mount.path)
        })
        .collect();
    let volumes = if volumes.is_empty() {
        String::new()
    } else {
        format!("\tvolumes {}", volumes.join(", "))
    };
    println!(
        "{}\t{}\t{}{volumes}",
        workspace.name,
        workspace.host,
        repositories.join(", ")
    );
}

fn print_volume(volume: &Volume) {
    let mut marks = vec![volume.kind.to_string()];
    if let Some(size) = volume.size_mib {
        marks.push(ByteSize::mib(size).to_string());
    }
    if let Some(path) = &volume.path {
        marks.push(format!("host path {path}"));
    }
    if !volume.hosts.is_empty() {
        marks.push(format!("on {}", volume.hosts.join(", ")));
    }
    if volume.deleting {
        marks.push("deletion pending".to_owned());
    }
    println!("{}\t{}", volume.name, marks.join(", "));
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
    let mut parts = vec![credentials];
    if !preset.model_accounts.is_empty() {
        let accounts: Vec<String> = preset
            .model_accounts
            .iter()
            .map(|(provider, account)| format!("{provider}={account}"))
            .collect();
        parts.push(format!("model accounts {}", accounts.join(", ")));
    }
    if !preset.secrets.is_empty() {
        let names: Vec<&str> = preset.secrets.keys().map(String::as_str).collect();
        parts.push(format!("secrets {}", names.join(", ")));
    }
    if !preset.files.is_empty() {
        parts.push(format!("{} skill files", preset.files.len()));
    }
    println!(
        "{}\t{}\tchanged {}",
        preset.name,
        parts.join("; "),
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
        Event::VolumeChanged { volume } => {
            print!("volume ");
            print_volume(volume);
        }
        Event::VolumeRemoved { name } => println!("volume {name} removed"),
        Event::ModelAccountChanged { account } => {
            print!("model account ");
            print_account(account);
        }
        Event::ModelAccountRemoved { name } => println!("model account {name} removed"),
        Event::ModelLoginFinished {
            login,
            account,
            error,
        } => match (account, error) {
            (Some(account), _) => println!("sign-in {login} finished: {}", account.name),
            (None, Some(error)) => println!("sign-in {login} failed: {error}"),
            (None, None) => println!("sign-in {login} finished"),
        },
        Event::SecretChanged { secret } => {
            print!("secret ");
            print_secret(secret);
        }
        Event::SecretRemoved { name } => println!("secret {name} removed"),
        Event::WorkspaceNotesChanged { workspace, .. } => {
            println!("the notes of workspace {workspace} changed")
        }
        Event::SessionChanged { session } => println!(
            "session {} of codespace {}: {}, {}",
            session.session.id,
            session.codespace,
            session.session.title.as_deref().unwrap_or("no title"),
            session.session.state
        ),
        Event::SessionRemoved { codespace, id } => {
            println!("session {id} of codespace {codespace} removed")
        }
        Event::ConfigChanged { presets } => {
            if presets.is_empty() {
                println!("the agent configurations changed")
            } else {
                println!("the presets {} changed", presets.join(", "))
            }
        }
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
    let me = identity(&dir, saved.identity.as_deref())?;
    let (client, mut events) = OracleClient::connect(&saved.endpoint()?, &me, CLIENT_NAME)
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
        Command::CheckHost { name, docker } => {
            let host = client.check_host(&name, docker).await?;
            if json {
                return print_json(&host.checks);
            }
            for check in &host.checks {
                println!("{}\t{}\t{}", check.status, check.name, check.detail);
            }
        }
        Command::SetSsh {
            host,
            destination,
            clear,
        } => {
            if destination.is_none() && !clear {
                bail!("give an SSH destination, or --clear to remove it");
            }
            let host = client.set_host_ssh(&host, destination.as_deref()).await?;
            if json {
                return print_json(&host);
            }
            match &host.ssh {
                Some(ssh) => println!("the host {} has the SSH destination {ssh}", host.name),
                None => println!("the host {} has no SSH destination", host.name),
            }
        }
        Command::HostVolumes { name } => {
            let volumes = client.host_volumes(&name).await?;
            if json {
                return print_json(&volumes);
            }
            for volume in &volumes {
                let mib = |bytes: u64| format!("{:.1} MiB", bytes as f64 / 1_048_576.0);
                let size = match (volume.used_bytes, volume.total_bytes, &volume.error) {
                    (_, _, Some(error)) => format!("error: {error}"),
                    (Some(used), Some(total), _) => format!("{} of {}", mib(used), mib(total)),
                    (Some(used), None, _) => mib(used),
                    (None, _, None) => "unknown".to_owned(),
                };
                println!("{}\t{size}", volume.name);
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
            volumes,
        } => {
            let workspace = client
                .create_workspace_with_volumes(&name, &host, repositories, volumes)
                .await?;
            if json {
                return print_json(&workspace);
            }
            print_workspace(&workspace);
        }
        Command::UpdateWorkspace {
            name,
            host,
            repositories,
            volumes,
            no_volumes,
        } => {
            let repositories = (!repositories.is_empty()).then_some(repositories);
            let volumes = (no_volumes || !volumes.is_empty()).then_some(volumes);
            let workspace = client
                .update_workspace(&name, host.as_deref(), repositories, volumes)
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
        Command::Volumes => {
            let volumes = client.volumes().await?;
            if json {
                return print_json(&volumes);
            }
            volumes.iter().for_each(print_volume);
        }
        Command::CreateVolume { name, size, path } => {
            let kind = if path.is_some() {
                VolumeKind::Directory
            } else {
                VolumeKind::Incus
            };
            let volume = client
                .create_volume(VolumeInput {
                    name,
                    kind,
                    size_mib: size,
                    path,
                })
                .await?;
            if json {
                return print_json(&volume);
            }
            print_volume(&volume);
        }
        Command::DeleteVolume { name } => {
            client.delete_volume(&name).await?;
            if !json {
                println!("deleted volume {name}");
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
            message,
            message_file,
            model,
            no_wait,
        } => {
            let text = match (message, message_file) {
                (Some(text), _) => Some(text),
                (None, Some(path)) => Some(read_text(&path, "the first message")?),
                (None, None) => None,
            };
            if model.is_some() && text.is_none() {
                bail!("--model needs --message or --message-file");
            }
            let first_message = text.map(|text| FirstMessage { text, model });
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
                    first_message,
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
        Command::CloseCodespace {
            id,
            no_wait,
            push,
            discard,
            message,
            force,
        } => {
            let codespace = find_codespace(&client, &id).await?;
            match codespace.state {
                CodespaceState::Ready => {
                    let reached = async {
                        let agent = connect_codespace(&saved, &me, &codespace).await?;
                        let statuses = agent.repositories().await?;
                        Ok::<_, anyhow::Error>((agent, statuses))
                    };
                    match reached.await {
                        Ok((agent, statuses)) => {
                            guard_close(&agent, statuses, &push, &discard, message).await?;
                        }
                        Err(err) if force => {
                            eprintln!("warning: closing without the check: {err:#}");
                        }
                        Err(err) => {
                            return Err(err.context(
                                "cannot check the unsaved work; use --force to close the codespace anyway",
                            ));
                        }
                    }
                }
                CodespaceState::Lost if !force => bail!(
                    "the codespace is lost, so its agent cannot tell its unsaved work; use --force to close it"
                ),
                _ => {}
            }
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
        Command::SetPreset {
            name,
            file,
            agents_md,
            skills,
        } => {
            let yaml = std::fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let agents_md = agents_md
                .as_deref()
                .map(|path| read_text(path, "AGENTS.md"))
                .transpose()?;
            let files = skills.as_deref().map(read_skills).transpose()?;
            let preset = client
                .set_full_preset(&name, &yaml, agents_md.as_deref(), files)
                .await?;
            if json {
                return print_json(&preset);
            }
            print_preset(&preset);
        }
        Command::ShowPreset { name } => {
            let detail = client.preset(&name).await?;
            if json {
                return print_json(&detail);
            }
            print_preset(&detail.preset);
            println!("--- YAML\n{}", detail.preset.yaml.trim_end());
            if !detail.preset.agents_md.is_empty() {
                println!("--- AGENTS.md\n{}", detail.preset.agents_md.trim_end());
            }
            for file in &detail.files {
                println!("--- {}\n{}", file.path, file.content.trim_end());
            }
        }
        Command::DeletePreset { name } => {
            client.delete_preset(&name).await?;
            if !json {
                println!("deleted preset {name}");
            }
        }
        Command::Accounts => {
            let accounts = client.model_accounts().await?;
            if json {
                return print_json(&accounts);
            }
            accounts.iter().for_each(print_account);
        }
        Command::SignIn {
            account,
            provider,
            device_code,
        } => {
            client.subscribe().await?;
            let method = if device_code {
                ModelLoginMethod::DeviceCode
            } else {
                ModelLoginMethod::Browser { redirect_uri: None }
            };
            let login = client
                .start_model_login(&account, &provider, method)
                .await?;
            let account = match &login.prompt {
                ModelLoginPrompt::OpenUrl { url, .. } => {
                    eprintln!(
                        "Open this URL, sign in, and paste the code or the URL that you get:"
                    );
                    eprintln!("{url}");
                    let mut line = String::new();
                    std::io::stdin()
                        .read_line(&mut line)
                        .context("cannot read the code")?;
                    let line = line.trim().to_owned();
                    let input = if line.starts_with("http://") || line.starts_with("https://") {
                        ModelLoginInput::Redirect { url: line }
                    } else {
                        ModelLoginInput::Code { code: line }
                    };
                    client.complete_model_login(&login.login, input).await?
                }
                ModelLoginPrompt::DeviceCode {
                    user_code,
                    verification_uri,
                    ..
                } => {
                    eprintln!("Open {verification_uri} and enter the code {user_code}.");
                    loop {
                        let event = tokio::select! {
                            _ = tokio::signal::ctrl_c() => {
                                let _ = client.cancel_model_login(&login.login).await;
                                bail!("stopped the sign-in");
                            }
                            event = events.recv() => event,
                        };
                        match event {
                            None => bail!("lost the connection to the oracle"),
                            Some(Event::ModelLoginFinished {
                                login: finished,
                                account,
                                error,
                            }) if finished == login.login => match (account, error) {
                                (Some(account), _) => break account,
                                (None, error) => bail!(
                                    "the sign-in failed: {}",
                                    error.unwrap_or_else(|| "no reason".into())
                                ),
                            },
                            Some(_) => {}
                        }
                    }
                }
                ModelLoginPrompt::Unknown => bail!("the oracle asks for an unknown sign-in"),
            };
            if json {
                return print_json(&account);
            }
            print_account(&account);
        }
        Command::SetApiKey {
            account,
            provider,
            key_file,
        } => {
            let key = read_token(&key_file)?;
            let account = client
                .set_model_api_key(&account, &provider, key.expose())
                .await?;
            if json {
                return print_json(&account);
            }
            print_account(&account);
        }
        Command::DeleteAccount { name } => {
            client.delete_model_account(&name).await?;
            if !json {
                println!("deleted model account {name}");
            }
        }
        Command::Models { preset } => {
            let models = client.models(&preset).await?;
            if json {
                return print_json(&models);
            }
            for error in &models.errors {
                eprintln!("warning: {error}");
            }
            for model in &models.models {
                let default = models.default_model.as_deref() == Some(model.id.as_str());
                println!(
                    "{}\t{}{}",
                    model.id,
                    model.account.as_deref().unwrap_or("-"),
                    if default { "\tdefault" } else { "" }
                );
            }
        }
        Command::Secrets => {
            let secrets = client.secrets().await?;
            if json {
                return print_json(&secrets);
            }
            secrets.iter().for_each(print_secret);
        }
        Command::SetSecret { name, value_file } => {
            let value = read_text(&value_file, "the secret")?;
            let value = value.strip_suffix('\n').unwrap_or(&value);
            let secret = client.set_secret(&name, value).await?;
            if json {
                return print_json(&secret);
            }
            print_secret(&secret);
        }
        Command::DeleteSecret { name } => {
            client.delete_secret(&name).await?;
            if !json {
                println!("deleted secret {name}");
            }
        }
        Command::Notes { workspace } => {
            let notes = client.workspace_notes(&workspace).await?;
            if json {
                return print_json(&notes);
            }
            print!("{}", notes.text);
        }
        Command::SetNotes { workspace, file } => {
            let text = read_text(&file, "the notes")?;
            let notes = client.set_workspace_notes(&workspace, &text).await?;
            if json {
                return print_json(&notes);
            }
            println!("set the notes of workspace {}", notes.workspace);
        }
        Command::Sessions { codespace } => {
            let sessions = client.sessions(codespace.as_deref()).await?;
            if json {
                return print_json(&sessions);
            }
            for entry in &sessions {
                let session = &entry.session;
                println!(
                    "{}\t{}\t{}\t{}{}\tchanged {}",
                    entry.codespace,
                    session.id,
                    session.state,
                    session.title.as_deref().unwrap_or("-"),
                    if session.archived { " (archived)" } else { "" },
                    time(session.updated_at)
                );
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
            let codespace = find_codespace(&client, &id).await?;
            print!("{}", client_entry(&saved, &codespace, name)?);
        }
        Command::Repositories { id } => {
            let codespace = find_codespace(&client, &id).await?;
            let agent = connect_codespace(&saved, &me, &codespace).await?;
            let statuses = agent.repositories().await?;
            if json {
                return print_json(&statuses);
            }
            statuses.iter().for_each(print_repository);
        }
        Command::Push {
            id,
            message,
            directories,
        } => {
            let codespace = find_codespace(&client, &id).await?;
            let agent = connect_codespace(&saved, &me, &codespace).await?;
            let statuses = agent
                .push_repositories(PushRepositories {
                    commit_message: message,
                    directories,
                })
                .await?;
            if json {
                return print_json(&statuses);
            }
            statuses.iter().for_each(print_repository);
            if statuses.iter().any(|s| s.error.is_some()) {
                bail!("cannot push all repositories");
            }
        }
        Command::PullRequest {
            id,
            title,
            body,
            body_file,
            draft,
        } => {
            let body = match body_file {
                Some(path) => std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?,
                None => body,
            };
            let codespace = find_codespace(&client, &id).await?;
            let agent = connect_codespace(&saved, &me, &codespace).await?;
            let results = agent.open_pull_requests(&title, &body, draft).await?;
            if json {
                return print_json(&results);
            }
            for result in &results {
                let text = match (&result.url, &result.skipped, &result.error) {
                    (_, _, Some(error)) => format!("error: {error}"),
                    (Some(url), _, None) if result.created => format!("opened {url}"),
                    (Some(url), _, None) => format!("updated {url}"),
                    (None, Some(reason), None) => format!("skipped: {reason}"),
                    (None, None, None) => "nothing".to_owned(),
                };
                println!("{}\t{text}", result.directory);
            }
            if results.iter().any(|r| r.error.is_some()) {
                bail!("cannot open all pull requests");
            }
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
