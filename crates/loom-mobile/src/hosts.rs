//! The app's entry point: its identity, the hosts it paired with, and
//! connections to their agents.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use loom_client::proto::InstanceId;
use loom_client::{Client, ClientIdentity, RelayHost, pair_with_uri, update_agent_with};
use loom_crypto::{CLIENT_IDENTITY_FILE, decode_key, encode_key};
use loom_view_model::host_colors;

use crate::agent::AgentConnection;
use crate::error::{LoomError, Result};
use crate::records::{Agent, AgentUpdateInfo, Host, HostColor, PushNotice};
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
            color: None,
        };
        let mut hosts = self.lock();
        if let Some(known) = hosts.iter_mut().find(|h| h.host_id == host.host_id) {
            host.name = known.name.clone();
            host.color = known.color.clone();
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

    /// The colors to offer for hosts.
    pub fn host_palette(&self) -> Vec<HostColor> {
        host_colors::PALETTE
            .iter()
            .map(|(name, color)| HostColor {
                name: (*name).to_owned(),
                color: (*color).to_owned(),
            })
            .collect()
    }

    /// Sets the color that marks a host's sessions, as `#rrggbb`; `None`
    /// takes it away.
    pub fn set_host_color(&self, name: String, color: Option<String>) -> Result<()> {
        let color = color.map(|c| c.trim().to_ascii_lowercase());
        if let Some(color) = &color
            && host_colors::parse(color).is_none()
        {
            return Err(LoomError::invalid(format!(
                "{color} is not a #rrggbb color"
            )));
        }
        let mut hosts = self.lock();
        host_mut(&mut hosts, &name)?.color = color;
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

    /// Opens a notification from a paired host's agent: `host_id` and
    /// `payload` are the `h` and `p` of the push message. Fails for a host
    /// that is not paired, and for one that did not seal it for this device.
    pub fn open_push(&self, host_id: String, payload: String) -> Result<PushNotice> {
        use base64::Engine;
        let (name, host_key) = {
            let hosts = self.lock();
            let host = hosts
                .iter()
                .find(|h| h.host_id.eq_ignore_ascii_case(host_id.trim()))
                .ok_or_else(|| {
                    LoomError::invalid("a notification from a host that is not paired")
                })?;
            (host.name.clone(), host.host_key.clone())
        };
        let host_key =
            decode_key(&host_key).ok_or_else(|| LoomError::invalid("the host's key is invalid"))?;
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .map_err(|_| LoomError::invalid("a malformed notification"))?;
        let plaintext = loom_crypto::open_push(&self.identity, &host_key, &sealed)
            .map_err(|_| LoomError::invalid("a notification that this device cannot open"))?;
        let notice: loom_client::proto::push::PushNotice = loom_client::proto::decode(&plaintext)
            .map_err(|_| {
            LoomError::invalid("a notification of a version this app does not know")
        })?;
        Ok(PushNotice::of(&name, notice))
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

    pub async fn update_agents(
        self: Arc<Self>,
        host: String,
        version: Option<String>,
    ) -> Result<AgentUpdateInfo> {
        let relay = self.relay(&host)?;
        let identity = self.identity.clone();
        run(async move {
            let instances = relay.list_agents().await?;
            let first = instances
                .into_iter()
                .next()
                .ok_or_else(|| LoomError::invalid(format!("no agent runs on {host}")))?;
            let connect = || relay.connect(Some(&first), &identity);
            Ok(update_agent_with(connect, CLIENT_NAME, version)
                .await?
                .into())
        })
        .await
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
            color: None,
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

    #[test]
    fn host_colors_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        // Hosts saved before hosts had colors.
        std::fs::write(
            dir.path().join(HOSTS_FILE),
            r#"[{"name":"box","relay_url":"wss://relay","host_id":"a","host_key":"k","instance":null}]"#,
        )
        .unwrap();
        let loom = Loom::new(path.clone()).unwrap();
        assert_eq!(loom.hosts(), [host("box", "a")]);
        assert!(
            loom.set_host_color("box".into(), Some("blue".into()))
                .is_err()
        );
        assert!(
            loom.set_host_color("nope".into(), Some("#3b82f6".into()))
                .is_err()
        );
        loom.set_host_color("box".into(), Some("#3B82F6".into()))
            .unwrap();
        let again = Loom::new(path).unwrap();
        assert_eq!(again.hosts()[0].color.as_deref(), Some("#3b82f6"));
        again.rename_host("box".into(), "lab".into()).unwrap();
        assert_eq!(again.hosts()[0].color.as_deref(), Some("#3b82f6"));
        again.set_host_color("lab".into(), None).unwrap();
        assert_eq!(again.hosts(), [host("lab", "a")]);
        assert_eq!(again.host_palette().len(), host_colors::PALETTE.len());
    }
}
