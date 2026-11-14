//! The app's entry point: its identity, the hosts it paired with, and
//! connections to their agents.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use loom_client::proto::InstanceId;
use loom_client::{Client, ClientIdentity, RelayHost, pair_with_uri};
use loom_crypto::{CLIENT_IDENTITY_FILE, encode_key};

use crate::agent::AgentConnection;
use crate::error::{LoomError, Result};
use crate::records::{Agent, Host};
use crate::runtime::run;

/// The name the core gives itself when it connects to an agent.
pub const CLIENT_NAME: &str = "loom-android";
const HOSTS_FILE: &str = "hosts.json";

/// The core of the app, made once with the app's private files directory.
#[derive(uniffi::Object)]
pub struct Loom {
    dir: PathBuf,
    identity: Arc<ClientIdentity>,
    hosts: Mutex<Vec<Host>>,
}

#[uniffi::export]
impl Loom {
    /// Opens the core's files in `data_dir`, making the device's key on the
    /// first run.
    #[uniffi::constructor]
    pub fn new(data_dir: String) -> Result<Arc<Self>> {
        crate::runtime::install_crypto();
        let dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&dir).map_err(LoomError::storage)?;
        let identity = ClientIdentity::load_or_create(&dir.join(CLIENT_IDENTITY_FILE))
            .map_err(LoomError::storage)?;
        let hosts = load_hosts(&dir)?;
        Ok(Arc::new(Self {
            dir,
            identity: Arc::new(identity),
            hosts: Mutex::new(hosts),
        }))
    }

    /// This device's public key, as `loom-agent clients list` shows it.
    pub fn device_key(&self) -> String {
        encode_key(&self.identity.public_key())
    }

    pub fn hosts(&self) -> Vec<Host> {
        self.lock().clone()
    }

    /// Pairs with a host using the `loom://` link that `loom-agent pair`
    /// prints, and keeps it as `name`. `label` names this device on the
    /// host. Pairing again with a known host updates it under its old name.
    pub async fn pair(self: Arc<Self>, link: String, label: String, name: String) -> Result<Host> {
        let identity = self.identity.clone();
        let (relay, instance) =
            run(async move { pair_with_uri(link.trim(), &identity, label.trim(), None).await })
                .await
                .map_err(|err| match err {
                    loom_client::ClientError::Crypto(err) => LoomError::invalid(err.to_string()),
                    err => err.into(),
                })?;
        let mut host = Host {
            name: name.trim().to_owned(),
            relay_url: relay.url.clone(),
            host_id: relay.host.to_string(),
            host_key: relay.host_key_text(),
            instance: Some(instance.to_string()),
        };
        let mut hosts = self.lock();
        if let Some(known) = hosts.iter_mut().find(|h| h.host_id == host.host_id) {
            host.name = known.name.clone();
            *known = host.clone();
        } else {
            host.name = unique_name(&hosts, &host.name, &host.host_id);
            hosts.push(host.clone());
        }
        save_hosts(&self.dir, &hosts)?;
        Ok(host)
    }

    pub fn rename_host(&self, name: String, new_name: String) -> Result<()> {
        let new_name = new_name.trim().to_owned();
        if new_name.is_empty() {
            return Err(LoomError::invalid("a host needs a name"));
        }
        let mut hosts = self.lock();
        if hosts.iter().any(|h| h.name == new_name && h.name != name) {
            return Err(LoomError::invalid(format!(
                "there is a host named {new_name}"
            )));
        }
        host_mut(&mut hosts, &name)?.name = new_name;
        save_hosts(&self.dir, &hosts)
    }

    /// Forgets a host. The host still lists this device until someone runs
    /// `loom-agent clients revoke` there.
    pub fn remove_host(&self, name: String) -> Result<()> {
        let mut hosts = self.lock();
        let before = hosts.len();
        hosts.retain(|h| h.name != name);
        if hosts.len() == before {
            return Err(unknown_host(&name));
        }
        save_hosts(&self.dir, &hosts)
    }

    /// The agents running on a host. Their workspaces are known when one of
    /// them could be asked; otherwise only their instance IDs.
    pub async fn list_agents(self: Arc<Self>, host: String) -> Result<Vec<Agent>> {
        let relay = self.relay(&host)?;
        let identity = self.identity.clone();
        run(async move {
            let instances = relay.list_agents().await?;
            let Some(first) = instances.first() else {
                return Ok(Vec::new());
            };
            let detailed = async {
                let stream = relay.connect(Some(first), &identity).await?;
                let (client, _notifications) = Client::connect(stream, CLIENT_NAME).await?;
                let mut infos = vec![client.agent().clone()];
                // Its siblings, when it supervises them.
                match client.list_agents().await {
                    Ok(more) => infos.extend(more),
                    Err(err) => tracing::debug!(error = %err, "could not list sibling agents"),
                }
                Ok::<_, loom_client::ClientError>(infos)
            };
            let mut agents: Vec<Agent> = match detailed.await {
                Ok(infos) => {
                    let mut agents: Vec<Agent> = Vec::new();
                    for info in &infos {
                        if !agents.iter().any(|a| a.instance == info.instance.0) {
                            agents.push(Agent::from(info));
                        }
                    }
                    agents
                }
                Err(err) => {
                    tracing::debug!(error = %err, "could not reach an agent to describe it");
                    Vec::new()
                }
            };
            // Agents the relay knows of but the one asked did not list.
            for instance in &instances {
                if !agents.iter().any(|a| a.instance == instance.0) {
                    agents.push(Agent {
                        instance: instance.to_string(),
                        workspace: None,
                        version: None,
                        default_model: None,
                    });
                }
            }
            agents.sort_by(|a, b| a.workspace.cmp(&b.workspace));
            Ok(agents)
        })
        .await
    }

    /// Connects to an agent on a host. The connection then follows the
    /// agent's sessions; see [`AgentConnection::next_updates`].
    pub async fn connect(
        self: Arc<Self>,
        host: String,
        instance: String,
    ) -> Result<Arc<AgentConnection>> {
        let relay = self.relay(&host)?;
        AgentConnection::open(relay, InstanceId(instance), self.identity.clone()).await
    }
}

impl Loom {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Host>> {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn relay(&self, name: &str) -> Result<RelayHost> {
        let hosts = self.lock();
        let host = hosts
            .iter()
            .find(|h| h.name == name)
            .ok_or_else(|| unknown_host(name))?;
        RelayHost::from_parts(&host.relay_url, &host.host_id, &host.host_key, None)
            .map_err(|err| LoomError::invalid(err.to_string()))
    }
}

fn unknown_host(name: &str) -> LoomError {
    LoomError::invalid(format!("there is no host named {name}"))
}

fn host_mut<'a>(hosts: &'a mut [Host], name: &str) -> Result<&'a mut Host> {
    hosts
        .iter_mut()
        .find(|h| h.name == name)
        .ok_or_else(|| unknown_host(name))
}

/// `name`, or the start of the host ID when it is empty, made unique.
fn unique_name(hosts: &[Host], name: &str, host_id: &str) -> String {
    let base = if name.is_empty() {
        host_id.chars().take(8).collect()
    } else {
        name.to_owned()
    };
    let taken = |candidate: &str| hosts.iter().any(|h| h.name == candidate);
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|candidate| !taken(candidate))
        .expect("some suffix is free")
}

fn load_hosts(dir: &Path) -> Result<Vec<Host>> {
    match std::fs::read(dir.join(HOSTS_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(LoomError::storage),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(LoomError::storage(err)),
    }
}

fn save_hosts(dir: &Path, hosts: &[Host]) -> Result<()> {
    let tmp = dir.join(format!("{HOSTS_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(hosts).map_err(LoomError::storage)?;
    std::fs::write(&tmp, bytes).map_err(LoomError::storage)?;
    std::fs::rename(&tmp, dir.join(HOSTS_FILE)).map_err(LoomError::storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, id: &str) -> Host {
        Host {
            name: name.into(),
            relay_url: "wss://relay".into(),
            host_id: id.into(),
            host_key: "k".into(),
            instance: None,
        }
    }

    #[test]
    fn the_core_picks_the_tls_crypto() {
        let dir = tempfile::tempdir().unwrap();
        Loom::new(dir.path().to_string_lossy().into_owned()).unwrap();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn host_names_stay_unique() {
        let hosts = vec![host("box", "a"), host("box 2", "b")];
        assert_eq!(unique_name(&hosts, "lab", "c"), "lab");
        assert_eq!(unique_name(&hosts, "box", "c"), "box 3");
        assert_eq!(unique_name(&hosts, "", "0123456789"), "01234567");
    }

    #[test]
    fn hosts_survive_a_restart_and_the_key_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let loom = Loom::new(path.clone()).unwrap();
        let key = loom.device_key();
        save_hosts(dir.path(), &[host("box", "a")]).unwrap();

        let again = Loom::new(path).unwrap();
        assert_eq!(again.device_key(), key);
        assert_eq!(again.hosts(), [host("box", "a")]);
        again.rename_host("box".into(), " lab ".into()).unwrap();
        assert!(again.rename_host("nope".into(), "x".into()).is_err());
        assert!(again.rename_host("lab".into(), "".into()).is_err());
        assert_eq!(load_hosts(dir.path()).unwrap()[0].name, "lab");
        again.remove_host("lab".into()).unwrap();
        assert!(again.hosts().is_empty());
        assert!(again.remove_host("lab".into()).is_err());
    }
}
