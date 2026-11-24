use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use loom_client::oracle_proto::{AgentAddress, ORACLE_INSTANCE};
use loom_client::proto::{AgentInfo, AgentUpdate, InstanceId, PROTOCOL_VERSION};
use loom_client::{
    AccessToken, AgentTarget, Client, ClientError, ClientIdentity, FrameStream, Notifications,
    OracleClient, OracleEndpoint, OracleEvents, RelayHost, SshTarget, can_update_agent,
    connect_command, connect_local, connect_ssh, list_local_agents, list_remote_agents,
    load_client_identity, local_registry, pair_oracle, pair_with_uri, update_agent_of_protocol,
};
use loom_config::{AgentConfig, CloudflareAccessConfig, Dirs, HostEntry};

pub const CLIENT_NAME: &str = "loom-gui";
const SSH_CONNECT_TIMEOUT: &str = "ConnectTimeout=5";
const SSH_HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const SSH_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshRoute {
    pub destination: String,
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodespaceRoute {
    pub address: AgentAddress,
    pub ssh: Option<SshRoute>,
}

type RouteKey = (String, String);

#[derive(Debug, Clone, Default)]
pub struct CodespaceBook {
    agents: Arc<Mutex<HashMap<RouteKey, CodespaceRoute>>>,
    ssh_failed: Arc<Mutex<HashMap<RouteKey, Instant>>>,
}

impl CodespaceBook {
    pub fn set(&self, host: &str, routes: Vec<CodespaceRoute>) {
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        agents.retain(|(name, _), _| name != host);
        for route in routes {
            if let Some(instance) = route.address.instance.clone() {
                agents.insert((host.to_owned(), instance), route);
            }
        }
    }

    pub fn get(&self, host: &str, instance: &str) -> Option<AgentAddress> {
        self.route(host, instance).map(|route| route.address)
    }

    pub fn route(&self, host: &str, instance: &str) -> Option<CodespaceRoute> {
        let agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        agents.get(&(host.to_owned(), instance.to_owned())).cloned()
    }

    fn ssh_route(&self, host: &str, instance: &str) -> Option<SshRoute> {
        let key = (host.to_owned(), instance.to_owned());
        let failed = self.ssh_failed.lock().unwrap_or_else(|e| e.into_inner());
        if failed
            .get(&key)
            .is_some_and(|at| at.elapsed() < SSH_RETRY_AFTER)
        {
            return None;
        }
        drop(failed);
        self.route(host, instance)?.ssh
    }

    fn ssh_result(&self, host: &str, instance: &str, worked: bool) {
        let key = (host.to_owned(), instance.to_owned());
        let mut failed = self.ssh_failed.lock().unwrap_or_else(|e| e.into_inner());
        if worked {
            failed.remove(&key);
        } else {
            failed.insert(key, Instant::now());
        }
    }
}

#[derive(Debug, Clone)]
pub struct Env {
    pub dirs: Dirs,
    pub agent_bin: PathBuf,
    pub codespaces: CodespaceBook,
}

impl Env {
    pub fn from_env() -> anyhow::Result<Self> {
        let dirs = Dirs::from_env()?;
        let agent_bin = std::env::var_os("LOOM_AGENT_BIN")
            .map(PathBuf::from)
            .or_else(sibling_agent_bin)
            .unwrap_or_else(|| PathBuf::from("loom-agent"));
        Ok(Self {
            dirs,
            agent_bin,
            codespaces: CodespaceBook::default(),
        })
    }

    fn agent_config(&self) -> Result<AgentConfig, String> {
        let path = self
            .dirs
            .agent_config_path(None, std::env::var_os("LOOM_CONFIG"));
        AgentConfig::load(&path, &self.dirs).map_err(|e| e.to_string())
    }

    fn ssh_target(&self, destination: &str, remote_bin: Option<&str>) -> Result<SshTarget, String> {
        let mut ssh = SshTarget::new(destination);
        if let Some(bin) = remote_bin {
            ssh.remote_bin = bin.to_owned();
        }
        let control = self.dirs.default_runtime_dir().join("ssh");
        std::fs::create_dir_all(&control).map_err(|e| e.to_string())?;
        ssh.control_dir = Some(control);
        Ok(ssh)
    }

    pub fn identity(&self) -> Result<ClientIdentity, String> {
        load_client_identity(&self.dirs).map_err(|e| e.to_string())
    }
}

fn sibling_agent_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("loom-agent");
    candidate.is_file().then_some(candidate)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub instance: String,
    pub workspace: Option<PathBuf>,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Instance(String),
    Workspace(PathBuf),
    Attach(PathBuf),
}

fn access_token(access: &Option<CloudflareAccessConfig>) -> Result<Option<AccessToken>, String> {
    match access {
        Some(access) => AccessToken::from_env(&access.client_id_env, &access.client_secret_env)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

fn relay_host(entry: &HostEntry) -> Result<Option<RelayHost>, String> {
    match entry {
        HostEntry::Relay {
            url,
            host_id,
            host_key,
            cloudflare_access,
            ..
        } => RelayHost::from_parts(url, host_id, host_key, access_token(cloudflare_access)?)
            .map(Some)
            .map_err(|e| e.to_string()),
        _ => Ok(None),
    }
}

pub fn oracle_endpoint(entry: &HostEntry) -> Result<OracleEndpoint, String> {
    match entry {
        HostEntry::Oracle {
            url,
            host_id,
            host_key,
            cloudflare_access,
            ..
        } => OracleEndpoint::from_parts(url, host_id, host_key, access_token(cloudflare_access)?)
            .map_err(|e| e.to_string()),
        _ => Err(format!("{} is not an oracle", entry.name())),
    }
}

pub async fn connect_oracle(
    env: &Env,
    entry: &HostEntry,
) -> Result<(OracleClient, OracleEvents), String> {
    let endpoint = oracle_endpoint(entry)?;
    let identity = env.identity()?;
    let (client, events) = OracleClient::connect(&endpoint, &identity, CLIENT_NAME)
        .await
        .map_err(|e| match e {
            ClientError::Oracle(err) => err.message,
            e => e.to_string(),
        })?;
    client.subscribe().await.map_err(err)?;
    Ok((client, events))
}

fn err(e: ClientError) -> String {
    e.to_string()
}

pub async fn list_agents(env: &Env, entry: &HostEntry) -> Result<Vec<AgentSummary>, String> {
    let from_entries = |entries: Vec<loom_client::AgentEntry>| {
        let mut agents: Vec<AgentSummary> = entries
            .into_iter()
            .map(|e| AgentSummary {
                instance: e.instance,
                workspace: Some(e.workspace),
                pid: Some(e.pid),
            })
            .collect();
        agents.sort_by(|a, b| a.workspace.cmp(&b.workspace));
        agents
    };
    match entry {
        HostEntry::Local { .. } => {
            let config = env.agent_config()?;
            let registry = local_registry(&env.dirs, &config);
            list_local_agents(&registry).map(from_entries).map_err(err)
        }
        HostEntry::Ssh {
            destination,
            remote_bin,
            ..
        } => {
            let ssh = env.ssh_target(destination, remote_bin.as_deref())?;
            list_remote_agents(&ssh)
                .await
                .map(from_entries)
                .map_err(err)
        }
        HostEntry::Relay { .. } => {
            let Some(relay) = relay_host(entry)? else {
                return Ok(Vec::new());
            };
            let instances = relay.list_agents().await.map_err(err)?;
            Ok(instances
                .into_iter()
                .map(|i| AgentSummary {
                    instance: i.to_string(),
                    workspace: None,
                    pid: None,
                })
                .collect())
        }
        HostEntry::Oracle { .. } => Ok(Vec::new()),
    }
}

async fn open(env: &Env, entry: &HostEntry, target: &Target) -> Result<FrameStream, String> {
    Ok(match entry {
        HostEntry::Local { .. } => {
            let config = env.agent_config()?;
            let registry = local_registry(&env.dirs, &config);
            connect_local(&registry, &agent_target(target), &env.agent_bin)
                .await
                .map_err(err)?
        }
        HostEntry::Ssh {
            destination,
            remote_bin,
            ..
        } => {
            let ssh = env.ssh_target(destination, remote_bin.as_deref())?;
            connect_ssh(&ssh, &agent_target(target)).map_err(err)?
        }
        HostEntry::Relay { instance, .. } => {
            let relay = relay_host(entry)?.ok_or("not a relay host")?;
            let identity = env.identity()?;
            match target {
                Target::Instance(id) => {
                    let id = InstanceId(id.clone());
                    relay.connect(Some(&id), &identity).await.map_err(err)?
                }
                Target::Attach(_) => relay
                    .connect(instance.clone().map(InstanceId).as_ref(), &identity)
                    .await
                    .map_err(err)?,
                Target::Workspace(path) => {
                    let preferred = instance.clone().map(InstanceId);
                    let stream = relay
                        .connect(preferred.as_ref(), &identity)
                        .await
                        .map_err(err)?;
                    let (client, _notes) =
                        Client::connect(stream, CLIENT_NAME).await.map_err(err)?;
                    let spawned = client.spawn_agent(path.clone()).await.map_err(err)?;
                    relay
                        .connect(Some(&spawned.instance), &identity)
                        .await
                        .map_err(err)?
                }
            }
        }
        HostEntry::Oracle { .. } => {
            let Target::Instance(instance) = target else {
                return Err("a codespace of the oracle starts only from a workspace".into());
            };
            let address = env
                .codespaces
                .get(entry.name(), instance)
                .ok_or("the oracle does not list this codespace agent")?;
            oracle_endpoint(entry)?
                .connect_codespace(&address, &env.identity()?)
                .await
                .map_err(err)?
        }
    })
}

async fn connect_codespace_ssh(
    env: &Env,
    route: &SshRoute,
    instance: &str,
) -> Result<(Client, Notifications), String> {
    let Some((program, args)) = route.command.split_first() else {
        return Err("the oracle gave no SSH command".into());
    };
    let mut ssh = env.ssh_target(&route.destination, Some(program))?;
    ssh.extra_args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        SSH_CONNECT_TIMEOUT.into(),
    ];
    let stream = connect_command(ssh.command(args)).map_err(|e| e.to_string())?;
    let (client, notifications) =
        tokio::time::timeout(SSH_HELLO_TIMEOUT, Client::connect(stream, CLIENT_NAME))
            .await
            .map_err(|_| "the SSH connection took too long".to_owned())?
            .map_err(err)?;
    if client.agent().instance.0 != instance {
        return Err(format!(
            "SSH reached the agent {}, not {instance}",
            client.agent().instance.0
        ));
    }
    Ok((client, notifications))
}

pub async fn connect(
    env: &Env,
    entry: &HostEntry,
    target: &Target,
) -> Result<(Client, Notifications), String> {
    if let (HostEntry::Oracle { .. }, Target::Instance(instance)) = (entry, target)
        && let Some(route) = env.codespaces.ssh_route(entry.name(), instance)
    {
        match connect_codespace_ssh(env, &route, instance).await {
            Ok(connected) => {
                tracing::info!(destination = %route.destination, %instance, "connected to a codespace over SSH");
                env.codespaces.ssh_result(entry.name(), instance, true);
                return Ok(connected);
            }
            Err(error) => {
                tracing::info!(destination = %route.destination, %instance, %error, "SSH to the codespace failed; using the relay");
                env.codespaces.ssh_result(entry.name(), instance, false);
            }
        }
    }
    let stream = open(env, entry, target).await?;
    Client::connect(stream, CLIENT_NAME)
        .await
        .map_err(|e| match e.agent_protocol() {
            Some(protocol) if protocol < PROTOCOL_VERSION && can_update_agent(protocol) => {
                format!(
                    "{e}. Update the agents of {} with its download button under Remotes.",
                    entry.name()
                )
            }
            _ => err(e),
        })
}

pub async fn update_agents(
    env: &Env,
    entry: &HostEntry,
    instance: &str,
    version: Option<String>,
) -> Result<AgentUpdate, String> {
    let target = Target::Instance(instance.to_owned());
    let stream = open(env, entry, &target).await?;
    match Client::connect(stream, CLIENT_NAME).await {
        Ok((client, _notifications)) => client.update_agent(version).await.map_err(err),
        Err(e) => match e.agent_protocol() {
            Some(protocol) => {
                let stream = open(env, entry, &target).await?;
                update_agent_of_protocol(stream, CLIENT_NAME, protocol, version)
                    .await
                    .map_err(err)
            }
            None => Err(err(e)),
        },
    }
}

fn agent_target(target: &Target) -> AgentTarget {
    match target {
        Target::Instance(id) => AgentTarget::Instance(id.clone()),
        Target::Workspace(path) => AgentTarget::Workspace {
            path: path.clone(),
            spawn: true,
        },
        Target::Attach(path) => AgentTarget::Workspace {
            path: path.clone(),
            spawn: false,
        },
    }
}

pub async fn pair(env: &Env, uri: &str, label: &str, name: &str) -> Result<HostEntry, String> {
    let identity = env.identity()?;
    let uri = uri.trim();
    let oracle = uri
        .parse::<loom_client::PairingUri>()
        .is_ok_and(|parsed| parsed.instance.0 == ORACLE_INSTANCE);
    if oracle {
        let endpoint = pair_oracle(uri, &identity, label, None)
            .await
            .map_err(err)?;
        return Ok(HostEntry::Oracle {
            name: name.to_owned(),
            url: endpoint.url.clone(),
            host_id: endpoint.host.to_string(),
            host_key: endpoint.host_key_text(),
            cloudflare_access: None,
        });
    }
    let (host, instance) = pair_with_uri(uri, &identity, label, None)
        .await
        .map_err(err)?;
    Ok(HostEntry::Relay {
        name: name.to_owned(),
        url: host.url.clone(),
        host_id: host.host.to_string(),
        host_key: host.host_key_text(),
        instance: Some(instance.to_string()),
        cloudflare_access: None,
    })
}

pub fn describe(info: &AgentInfo) -> String {
    format!("{} ({})", info.workspace.display(), info.instance)
}

pub fn default_label() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "desktop".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let label: String = format!("{user}@{host}")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "-_.@".contains(*c))
        .take(64)
        .collect();
    if label.is_empty() {
        "loom-gui".into()
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_targets_spawn_agents() {
        assert_eq!(
            agent_target(&Target::Workspace("/w".into())),
            AgentTarget::Workspace {
                path: "/w".into(),
                spawn: true
            }
        );
        assert_eq!(
            agent_target(&Target::Attach("/w".into())),
            AgentTarget::Workspace {
                path: "/w".into(),
                spawn: false
            }
        );
        assert_eq!(
            agent_target(&Target::Instance("a".into())),
            AgentTarget::Instance("a".into())
        );
    }

    #[test]
    fn a_failed_ssh_route_waits_before_the_next_try() {
        let book = CodespaceBook::default();
        let address = AgentAddress {
            host_id: "h".into(),
            host_key: "k".into(),
            instance: Some("i1".into()),
        };
        let ssh = SshRoute {
            destination: "dev@box".into(),
            command: loom_client::oracle_proto::ssh_command("c1"),
        };
        book.set(
            "next",
            vec![CodespaceRoute {
                address: address.clone(),
                ssh: Some(ssh.clone()),
            }],
        );
        assert_eq!(book.get("next", "i1"), Some(address));
        assert_eq!(book.ssh_route("next", "i1"), Some(ssh.clone()));
        assert_eq!(book.ssh_route("other", "i1"), None);
        book.ssh_result("next", "i1", false);
        assert_eq!(book.ssh_route("next", "i1"), None);
        book.ssh_result("next", "i1", true);
        assert_eq!(book.ssh_route("next", "i1"), Some(ssh));
        book.set("next", Vec::new());
        assert_eq!(book.route("next", "i1"), None);
    }

    #[test]
    fn labels_are_valid() {
        let label = default_label();
        assert!(!label.is_empty() && label.len() <= 64);
        assert!(
            label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
        );
    }
}
