//! The app's entry point: its identity, the oracles it paired with, and
//! connections to the agents of their codespaces.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use loom_client::proto::InstanceId;
use loom_client::{ClientIdentity, OracleEndpoint, RelayHost, pair_oracle};
use loom_crypto::{CLIENT_IDENTITY_FILE, decode_key, encode_key};
use loom_view_model::host_colors;

use crate::agent::AgentConnection;
use crate::error::{LoomError, Result};
use crate::oracle::{KeyBook, OracleConnection};
use crate::records::{Host, HostColor, PushNotice};
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
    keys: Arc<KeyBook>,
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
        let keys = Arc::new(KeyBook::open(&dir));
        Ok(Arc::new(Self {
            dir,
            identity: Arc::new(identity),
            hosts: Mutex::new(hosts),
            keys,
        }))
    }

    /// This device's public key, as the oracle shows it.
    pub fn device_key(&self) -> String {
        encode_key(&self.identity.public_key())
    }

    pub fn hosts(&self) -> Vec<Host> {
        self.lock().clone()
    }

    /// Pairs with an oracle using a `loom://` pairing link, and keeps it as
    /// `name`. `label` names this device on the oracle. Pairing again with a
    /// known oracle updates it under its old name.
    pub async fn pair(self: Arc<Self>, link: String, label: String, name: String) -> Result<Host> {
        let identity = self.identity.clone();
        let crypto = |err: loom_client::ClientError| match err {
            loom_client::ClientError::Crypto(err) => LoomError::invalid(err.to_string()),
            err => err.into(),
        };
        let endpoint =
            run(async move { pair_oracle(link.trim(), &identity, label.trim(), None).await })
                .await
                .map_err(crypto)?;
        let mut host = Host {
            name: name.trim().to_owned(),
            relay_url: endpoint.url.clone(),
            host_id: endpoint.host.to_string(),
            host_key: endpoint.host_key_text(),
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
        host_mut(&mut hosts, &name)?.name = new_name.clone();
        save_hosts(&self.dir, &hosts)?;
        self.keys.rename(&name, &new_name);
        Ok(())
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

    /// Forgets an oracle. The oracle still lists this device until someone
    /// revokes it on the Devices page of the oracle settings.
    pub fn remove_host(&self, name: String) -> Result<()> {
        let mut hosts = self.lock();
        let before = hosts.len();
        hosts.retain(|h| h.name != name);
        if hosts.len() == before {
            return Err(unknown_host(&name));
        }
        save_hosts(&self.dir, &hosts)?;
        self.keys.set(&name, Vec::new());
        Ok(())
    }

    /// Opens a notification from the agent of a codespace: `host_id` and
    /// `payload` are the `h` and `p` of the push message. Fails for an
    /// agent that no paired oracle lists, and for one that did not seal it
    /// for this device.
    pub fn open_push(&self, host_id: String, payload: String) -> Result<PushNotice> {
        use base64::Engine;
        let key = self
            .keys
            .find(&host_id)
            .ok_or_else(|| LoomError::invalid("a notification from a host that is not paired"))?;
        let host_key = decode_key(&key.host_key)
            .ok_or_else(|| LoomError::invalid("the host's key is invalid"))?;
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(payload.trim())
            .map_err(|_| LoomError::invalid("a malformed notification"))?;
        let plaintext = loom_crypto::open_push(&self.identity, &host_key, &sealed)
            .map_err(|_| LoomError::invalid("a notification that this device cannot open"))?;
        let notice: loom_client::proto::push::PushNotice = loom_client::proto::decode(&plaintext)
            .map_err(|_| {
            LoomError::invalid("a notification of a version this app does not know")
        })?;
        let mut notice = PushNotice::of(&key.oracle, notice);
        notice.workspace = format!("{} · {}", key.workspace, key.branch);
        notice.codespace = Some(key.codespace);
        Ok(notice)
    }

    pub async fn connect_oracle(self: Arc<Self>, host: String) -> Result<Arc<OracleConnection>> {
        let endpoint = {
            let hosts = self.lock();
            let entry = hosts
                .iter()
                .find(|h| h.name == host)
                .ok_or_else(|| LoomError::invalid(format!("there is no oracle named {host}")))?;
            OracleEndpoint::from_parts(&entry.relay_url, &entry.host_id, &entry.host_key, None)
                .map_err(|err| LoomError::invalid(err.to_string()))?
        };
        OracleConnection::open(host, endpoint, self.identity.clone(), self.keys.clone()).await
    }

    /// Connects to the agent of a codespace of the oracle `host`. The
    /// connection then follows the agent's sessions; see
    /// [`AgentConnection::next_updates`].
    pub async fn connect(
        self: Arc<Self>,
        host: String,
        instance: String,
    ) -> Result<Arc<AgentConnection>> {
        let (relay, directories) = self.codespace_relay(&host, &instance)?;
        let connection =
            AgentConnection::open(relay, InstanceId(instance), self.identity.clone()).await?;
        connection.set_directories(directories);
        Ok(connection)
    }
}

impl Loom {
    #[doc(hidden)]
    pub fn add_test_codespace(
        &self,
        oracle: Host,
        host_id: String,
        host_key: String,
        instance: String,
    ) -> Result<()> {
        let name = oracle.name.clone();
        {
            let mut hosts = self.lock();
            hosts.retain(|h| h.name != name);
            hosts.push(oracle);
            save_hosts(&self.dir, &hosts)?;
        }
        self.keys.set(
            &name,
            vec![crate::oracle::CodespaceKey {
                oracle: name.clone(),
                codespace: instance.clone(),
                workspace: instance.clone(),
                branch: "main".into(),
                host_id,
                host_key,
                instance: Some(instance),
                repositories: Vec::new(),
            }],
        );
        Ok(())
    }

    fn codespace_relay(&self, name: &str, instance: &str) -> Result<(RelayHost, Vec<String>)> {
        let url = {
            let hosts = self.lock();
            hosts
                .iter()
                .find(|h| h.name == name)
                .ok_or_else(|| unknown_host(name))?
                .relay_url
                .clone()
        };
        let key = self
            .keys
            .of_instance(name, instance)
            .ok_or_else(|| LoomError::Offline {
                message: format!("{name} lists no codespace with the agent {instance}"),
            })?;
        let relay = RelayHost::from_parts(&url, &key.host_id, &key.host_key, None)
            .map_err(|err| LoomError::invalid(err.to_string()))?;
        Ok((relay, key.repositories))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Host>> {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner())
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
    let bytes = match std::fs::read(dir.join(HOSTS_FILE)) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(LoomError::storage(err)),
    };
    let stored: Vec<serde_json::Value> =
        serde_json::from_slice(&bytes).map_err(LoomError::storage)?;
    stored
        .into_iter()
        .filter(|host| host.get("oracle").and_then(serde_json::Value::as_bool) == Some(true))
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()
        .map_err(LoomError::storage)
}

fn save_hosts(dir: &Path, hosts: &[Host]) -> Result<()> {
    let tmp = dir.join(format!("{HOSTS_FILE}.tmp"));
    let mut stored = Vec::with_capacity(hosts.len());
    for host in hosts {
        let mut value = serde_json::to_value(host).map_err(LoomError::storage)?;
        if let Some(fields) = value.as_object_mut() {
            fields.insert("oracle".into(), true.into());
        }
        stored.push(value);
    }
    let bytes = serde_json::to_vec_pretty(&stored).map_err(LoomError::storage)?;
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
    fn notifications_of_codespaces_open_with_their_keys() {
        use base64::Engine;
        use loom_client::proto::push::{PUSH_NOTICE_VERSION, PushKind as Kind};

        let dir = tempfile::tempdir().unwrap();
        let loom = Loom::new(dir.path().to_string_lossy().into_owned()).unwrap();
        let agent = loom_crypto::HostIdentity::load_or_create(&dir.path().join("agent")).unwrap();
        let host_id = agent.host_id().to_string();
        loom.keys.set(
            "next",
            vec![crate::oracle::CodespaceKey {
                oracle: "next".into(),
                codespace: "c1".into(),
                workspace: "acme".into(),
                branch: "loom/c1".into(),
                host_id: host_id.clone(),
                host_key: encode_key(&agent.static_public()),
                instance: Some("i1".into()),
                repositories: vec!["app".into()],
            }],
        );
        let notice = loom_client::proto::push::PushNotice {
            version: PUSH_NOTICE_VERSION,
            instance: InstanceId("i1".into()),
            workspace: "/workspaces/acme".into(),
            session: loom_client::proto::SessionId("s1".into()),
            title: Some("Fix".into()),
            kind: Kind::Finished,
            detail: None,
            at: 5,
        };
        let plaintext = loom_client::proto::encode(&notice).unwrap();
        let sealed =
            loom_crypto::seal_push(&agent, &loom.identity.public_key(), &plaintext).unwrap();
        let payload = base64::engine::general_purpose::STANDARD.encode(sealed);
        let opened = loom.open_push(host_id.clone(), payload.clone()).unwrap();
        assert_eq!(opened.host, "next");
        assert_eq!(opened.codespace.as_deref(), Some("c1"));
        assert_eq!(opened.workspace, "acme · loom/c1");
        assert_eq!(opened.session, "s1");

        let again = Loom::new(dir.path().to_string_lossy().into_owned()).unwrap();
        assert!(again.open_push(host_id.clone(), payload.clone()).is_ok());
        again.keys.set("next", Vec::new());
        assert!(again.open_push(host_id, payload).is_err());
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
        // Hosts saved before hosts had colors, and a relay host of the
        // classic modes, which the app drops.
        std::fs::write(
            dir.path().join(HOSTS_FILE),
            r#"[{"name":"box","relay_url":"wss://relay","host_id":"a","host_key":"k","instance":"oracle","oracle":true},{"name":"old","relay_url":"wss://relay","host_id":"b","host_key":"k","instance":null}]"#,
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
