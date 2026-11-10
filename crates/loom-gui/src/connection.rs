use std::path::PathBuf;

use loom_client::proto::{AgentInfo, InstanceId};
use loom_client::{
    AccessToken, AgentTarget, Client, ClientError, ClientIdentity, Notifications, RelayHost,
    SshTarget, connect_local, connect_ssh, list_local_agents, list_remote_agents,
    load_client_identity, local_registry, pair_with_uri,
};
use loom_config::{AgentConfig, CloudflareAccessConfig, Dirs, HostEntry};

pub const CLIENT_NAME: &str = "loom-gui";

#[derive(Debug, Clone)]
pub struct Env {
    pub dirs: Dirs,
    pub agent_bin: PathBuf,
}

impl Env {
    pub fn from_env() -> anyhow::Result<Self> {
        let dirs = Dirs::from_env()?;
        let agent_bin = std::env::var_os("LOOM_AGENT_BIN")
            .map(PathBuf::from)
            .or_else(sibling_agent_bin)
            .unwrap_or_else(|| PathBuf::from("loom-agent"));
        Ok(Self { dirs, agent_bin })
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
    }
}

pub async fn connect(
    env: &Env,
    entry: &HostEntry,
    target: &Target,
) -> Result<(Client, Notifications), String> {
    let stream = match entry {
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
    };
    Client::connect(stream, CLIENT_NAME).await.map_err(err)
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
    let (host, instance) = pair_with_uri(uri.trim(), &identity, label, None)
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
