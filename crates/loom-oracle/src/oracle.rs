use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use loom_config::{Dirs, ORACLE_MASTER_KEY_FILE, OracleConfig};
use loom_crypto::{HostIdentity, PairingUri, Responder, Zeroizing, validate_label};
use loom_oracle_proto::{
    DEFAULT_PAIRING_TTL_SECS, Device, ErrorCode, Event, MAX_PAIRING_TTL_SECS, ORACLE_INSTANCE,
    OracleError, OracleInfo, PROTOCOL_VERSION, Pairing, PairingLink, RelayHost, Role,
};
use loom_proto::InstanceId;
use loom_proto::relay::{AgentId, HostId};
use loom_relay::{HostCheck, Relay};
use loom_transport::{IncomingChannel, LocalLink, serve_local_link};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::seal::MasterKey;
use crate::session::Session;
use crate::store::{PairingRow, PeerRow, RelayHostRow, Store, StoreError};

mod codespaces;
mod git;
mod models;
mod presets;
mod volumes;

pub use codespaces::{NewCodespaceArgs, valid_branch, validate_environment};
pub use git::{normalize_url, parse_preset, provider_path, resolve_repository, select_credential};

pub const STORE_FILE: &str = "oracle.db";
pub const CREDENTIALS_ENV: &str = "CREDENTIALS_DIRECTORY";
const EVENT_QUEUE: usize = 256;
const MAX_PUSH_TOKEN: usize = 4096;
const GRANT_TIMEOUT: Duration = Duration::from_secs(120);
const RELINK_DELAY: Duration = Duration::from_secs(1);

pub fn master_key_path(config: &OracleConfig, dirs: &Dirs, credentials: Option<&Path>) -> PathBuf {
    if config.master_key.is_none()
        && let Some(dir) = credentials
    {
        let path = dir.join(ORACLE_MASTER_KEY_FILE);
        if path.exists() {
            return path;
        }
    }
    config.master_key_path(dirs)
}

pub fn open_store(config: &OracleConfig, dirs: &Dirs) -> anyhow::Result<Store> {
    let data_dir = config.data_dir(dirs);
    let path = data_dir.join(STORE_FILE);
    let store = Store::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let credentials = std::env::var_os(CREDENTIALS_ENV).map(PathBuf::from);
    let key_path = master_key_path(config, dirs, credentials.as_deref());
    let key = match MasterKey::load(&key_path)? {
        Some(key) => key,
        None if !store.has_key()? => {
            let key = MasterKey::create(&key_path)?;
            tracing::info!(path = %key_path.display(), "created a master key");
            key
        }
        None => bail!(
            "the master key {} is missing; the store {} needs it",
            key_path.display(),
            path.display()
        ),
    };
    store
        .unlock(&key)
        .with_context(|| format!("cannot use the master key {}", key_path.display()))?;
    Ok(store)
}

pub fn pairing_link(url: &str, identity: &HostIdentity, secret: &Zeroizing<[u8; 32]>) -> String {
    PairingUri {
        relay_url: url.to_owned(),
        host: identity.host_id(),
        instance: InstanceId(ORACLE_INSTANCE.into()),
        host_static: identity.static_public(),
        secret: Zeroizing::new(**secret),
    }
    .to_string()
}

pub fn pairing_ttl(ttl_secs: Option<u64>) -> Result<Duration, OracleError> {
    match ttl_secs.unwrap_or(DEFAULT_PAIRING_TTL_SECS) {
        secs @ 1..=MAX_PAIRING_TTL_SECS => Ok(Duration::from_secs(secs)),
        _ => Err(OracleError::new(
            ErrorCode::BadRequest,
            format!("a pairing link lives 1 to {MAX_PAIRING_TTL_SECS} seconds"),
        )),
    }
}

pub struct RelayHosts {
    classic: RwLock<HashSet<HostId>>,
    codespaces: RwLock<HashSet<HostId>>,
}

fn write(set: &RwLock<HashSet<HostId>>) -> std::sync::RwLockWriteGuard<'_, HashSet<HostId>> {
    set.write().unwrap_or_else(|e| e.into_inner())
}

fn read(set: &RwLock<HashSet<HostId>>) -> std::sync::RwLockReadGuard<'_, HashSet<HostId>> {
    set.read().unwrap_or_else(|e| e.into_inner())
}

impl RelayHosts {
    fn new(
        classic: impl IntoIterator<Item = HostId>,
        codespaces: impl IntoIterator<Item = HostId>,
    ) -> Self {
        Self {
            classic: RwLock::new(classic.into_iter().collect()),
            codespaces: RwLock::new(codespaces.into_iter().collect()),
        }
    }

    fn insert(&self, host: HostId) {
        write(&self.classic).insert(host);
    }

    fn remove(&self, host: &HostId) {
        write(&self.classic).remove(host);
    }

    fn insert_codespace(&self, host: HostId) {
        write(&self.codespaces).insert(host);
    }

    fn remove_codespace(&self, host: &HostId) {
        write(&self.codespaces).remove(host);
    }
}

impl HostCheck for RelayHosts {
    fn check(&self, host: &HostId) -> Result<(), String> {
        if read(&self.classic).contains(host) || read(&self.codespaces).contains(host) {
            Ok(())
        } else {
            Err(format!(
                "host {host} is not a relay host or a codespace of the oracle"
            ))
        }
    }
}

impl From<StoreError> for OracleError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Conflict(message) => Self::new(ErrorCode::Conflict, message),
            other => {
                tracing::error!(error = %other, "the store failed");
                Self::new(ErrorCode::Internal, format!("the oracle failed: {other}"))
            }
        }
    }
}

fn not_found(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::NotFound, message)
}

fn bad_request(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::BadRequest, message)
}

fn pairing(row: &PairingRow) -> Pairing {
    Pairing {
        id: row.id.clone(),
        role: row.role,
        created_at: row.created_at,
        expires_at: row.expires_at,
        created_by: row.created_by,
    }
}

pub struct Oracle {
    store: Arc<Store>,
    identity: Arc<HostIdentity>,
    url: String,
    relay: Arc<Relay>,
    hosts: Arc<RelayHosts>,
    events: broadcast::Sender<Event>,
    sessions: Mutex<HashMap<u64, HashMap<u64, CancellationToken>>>,
    next_session: AtomicU64,
    grants: Mutex<HashMap<String, (Instant, PairingRow)>>,
    links: Mutex<HashMap<u64, codespaces::HostLink>>,
    calls: Mutex<HashMap<u64, codespaces::PendingCall>>,
    next_call: AtomicU64,
    workspace_lock: tokio::sync::Mutex<()>,
    volume_delete_lock: tokio::sync::Mutex<()>,
    codespace_lock: Mutex<()>,
    http: loom_github::Http,
    reach: Mutex<git::ReachCache>,
    models: models::ModelState,
}

impl Oracle {
    pub fn open(config: &OracleConfig, dirs: &Dirs) -> anyhow::Result<Arc<Self>> {
        let store = open_store(config, dirs)?;
        let data_dir = config.data_dir(dirs);
        let identity = HostIdentity::load_or_create(&data_dir)
            .with_context(|| format!("cannot read the keys in {}", data_dir.display()))?;
        Self::new(config, Arc::new(store), Arc::new(identity))
    }

    pub fn new(
        config: &OracleConfig,
        store: Arc<Store>,
        identity: Arc<HostIdentity>,
    ) -> anyhow::Result<Arc<Self>> {
        let hosts = Arc::new(RelayHosts::new(
            store.relay_hosts()?.into_iter().map(|row| row.host),
            store
                .codespaces()?
                .iter()
                .filter_map(codespaces::admitted_agent)
                .collect::<Vec<_>>(),
        ));
        let relay = Relay::with_host_check(config.relay_config(), hosts.clone());
        let (events, _) = broadcast::channel(EVENT_QUEUE);
        Ok(Arc::new(Self {
            store,
            identity,
            url: config.public_url(),
            relay,
            hosts,
            events,
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            grants: Mutex::new(HashMap::new()),
            links: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
            next_call: AtomicU64::new(1),
            workspace_lock: tokio::sync::Mutex::new(()),
            volume_delete_lock: tokio::sync::Mutex::new(()),
            codespace_lock: Mutex::new(()),
            http: loom_github::Http::new()?,
            reach: Mutex::new(HashMap::new()),
            models: models::ModelState::new(),
        }))
    }

    pub fn relay(&self) -> &Arc<Relay> {
        &self.relay
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn identity(&self) -> &Arc<HostIdentity> {
        &self.identity
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn agent_id(&self) -> AgentId {
        AgentId {
            host: self.identity.host_id(),
            instance: InstanceId(ORACLE_INSTANCE.into()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn audit(&self, by: Option<u64>, action: &str, detail: &str) {
        if let Err(err) = self.store.audit(by, action, detail) {
            tracing::error!(error = %err, action, "cannot write the audit log");
        }
    }

    pub fn start(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(run_control_agent(self.clone(), shutdown))
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<u64, HashMap<u64, CancellationToken>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn add_session(&self, peer: u64) -> (u64, CancellationToken) {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let kill = CancellationToken::new();
        self.lock_sessions()
            .entry(peer)
            .or_default()
            .insert(id, kill.clone());
        (id, kill)
    }

    fn remove_session(&self, peer: u64, id: u64) {
        let mut sessions = self.lock_sessions();
        if let Some(own) = sessions.get_mut(&peer) {
            own.remove(&id);
            if own.is_empty() {
                sessions.remove(&peer);
            }
        }
    }

    fn close_sessions(&self, peer: u64) {
        if let Some(own) = self.lock_sessions().remove(&peer) {
            for kill in own.values() {
                kill.cancel();
            }
        }
    }

    fn is_connected(&self, peer: u64) -> bool {
        self.lock_sessions().contains_key(&peer)
    }

    fn device(&self, row: &PeerRow) -> Device {
        Device {
            id: row.id,
            label: row.label.clone(),
            added_at: row.added_at,
            last_seen: row.last_seen,
            push: row.push_token.is_some(),
            connected: self.is_connected(row.id),
        }
    }

    fn device_changed(&self, id: u64) {
        match self.store.peer(id) {
            Ok(Some(row)) if row.role == Role::Device => self.emit(Event::DeviceChanged {
                device: self.device(&row),
            }),
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "cannot read a peer"),
        }
    }

    fn relay_host(&self, row: &RelayHostRow) -> RelayHost {
        RelayHost {
            host_id: row.host.to_string(),
            label: row.label.clone(),
            added_at: row.added_at,
            agents: self
                .relay
                .instances(&row.host)
                .into_iter()
                .map(|i| i.0)
                .collect(),
        }
    }

    pub fn info(&self) -> OracleInfo {
        OracleInfo {
            version: loom_version::VERSION.to_owned(),
            protocol: PROTOCOL_VERSION,
            url: self.url.clone(),
            host_id: self.identity.host_id().to_string(),
        }
    }

    pub fn devices(&self) -> Result<Vec<Device>, OracleError> {
        Ok(self
            .store
            .peers(Some(Role::Device))?
            .iter()
            .map(|row| self.device(row))
            .collect())
    }

    fn device_row(&self, id: u64) -> Result<PeerRow, OracleError> {
        match self.store.peer(id)? {
            Some(row) if row.role == Role::Device => Ok(row),
            _ => Err(not_found(format!("no device has the ID {id}"))),
        }
    }

    pub fn rename_device(
        &self,
        id: u64,
        label: &str,
        by: Option<u64>,
    ) -> Result<Device, OracleError> {
        validate_label(label).map_err(|e| bad_request(e.to_string()))?;
        let old = self.device_row(id)?;
        let row = self
            .store
            .rename_peer(id, label)?
            .ok_or_else(|| not_found(format!("no device has the ID {id}")))?;
        self.audit(
            by,
            "device_renamed",
            &format!("device {id} from {} to {label}", old.label),
        );
        let device = self.device(&row);
        self.emit(Event::DeviceChanged {
            device: device.clone(),
        });
        Ok(device)
    }

    pub fn revoke_device(&self, id: u64, by: Option<u64>) -> Result<(), OracleError> {
        self.device_row(id)?;
        let row = self
            .store
            .remove_peer(id)?
            .ok_or_else(|| not_found(format!("no device has the ID {id}")))?;
        self.close_sessions(id);
        self.audit(
            by,
            "device_revoked",
            &format!("device {id} ({})", row.label),
        );
        tracing::info!(device = %row.label, "revoked a device");
        self.emit(Event::DeviceRemoved { id });
        self.emit(Event::DeviceKeysChanged);
        Ok(())
    }

    pub fn set_push_token(&self, peer: u64, token: Option<&str>) -> Result<(), OracleError> {
        if let Some(token) = token
            && (token.is_empty()
                || token.len() > MAX_PUSH_TOKEN
                || !token.chars().all(|c| c.is_ascii_graphic()))
        {
            return Err(bad_request(format!(
                "a push token is 1 to {MAX_PUSH_TOKEN} printable ASCII characters"
            )));
        }
        let others = self
            .store
            .set_push_token(peer, token)?
            .ok_or_else(|| not_found("this peer is not paired any more"))?;
        let action = if token.is_some() {
            "push_token_set"
        } else {
            "push_token_removed"
        };
        self.audit(Some(peer), action, &format!("peer {peer}"));
        for id in others.into_iter().chain([peer]) {
            self.device_changed(id);
        }
        Ok(())
    }

    pub fn create_pairing(
        &self,
        role: Role,
        ttl: Duration,
        by: Option<u64>,
    ) -> Result<PairingLink, OracleError> {
        let (row, secret) = self.store.create_pairing(role, ttl, by)?;
        self.audit(by, "pairing_created", &format!("{role} pairing {}", row.id));
        let pairing = pairing(&row);
        self.emit(Event::PairingChanged {
            pairing: pairing.clone(),
        });
        Ok(PairingLink {
            pairing,
            link: pairing_link(&self.url, &self.identity, &secret),
        })
    }

    pub fn pairings(&self) -> Result<Vec<Pairing>, OracleError> {
        Ok(self.store.pairings()?.iter().map(pairing).collect())
    }

    pub fn cancel_pairing(&self, id: &str, by: Option<u64>) -> Result<(), OracleError> {
        if !self.store.cancel_pairing(id)? {
            return Err(not_found(format!("no pairing has the ID {id}")));
        }
        self.audit(by, "pairing_cancelled", &format!("pairing {id}"));
        self.emit(Event::PairingRemoved {
            id: id.to_owned(),
            used: false,
        });
        Ok(())
    }

    pub fn relay_hosts(&self) -> Result<Vec<RelayHost>, OracleError> {
        Ok(self
            .store
            .relay_hosts()?
            .iter()
            .map(|row| self.relay_host(row))
            .collect())
    }

    fn parse_host(host_id: &str) -> Result<HostId, OracleError> {
        host_id
            .parse()
            .map_err(|_| bad_request(format!("{host_id:?} is not a host ID of 64 hex characters")))
    }

    pub fn add_relay_host(
        &self,
        host_id: &str,
        label: &str,
        by: Option<u64>,
    ) -> Result<RelayHost, OracleError> {
        let host = Self::parse_host(host_id)?;
        validate_label(label).map_err(|e| bad_request(e.to_string()))?;
        if host == self.identity.host_id() {
            return Err(OracleError::new(
                ErrorCode::Conflict,
                "this is the host ID of the oracle itself",
            ));
        }
        let row = self.store.put_relay_host(&host, label)?;
        self.hosts.insert(host);
        self.audit(by, "relay_host_added", &format!("{host} ({label})"));
        tracing::info!(%host, %label, "added a relay host");
        let relay_host = self.relay_host(&row);
        self.emit(Event::RelayHostChanged {
            relay_host: relay_host.clone(),
        });
        Ok(relay_host)
    }

    pub fn remove_relay_host(&self, host_id: &str, by: Option<u64>) -> Result<(), OracleError> {
        let host = Self::parse_host(host_id)?;
        if !self.store.remove_relay_host(&host)? {
            return Err(not_found(format!("{host} is not a relay host")));
        }
        self.hosts.remove(&host);
        let agents = self.relay.disconnect_host(&host);
        self.audit(by, "relay_host_removed", &host.to_string());
        tracing::info!(%host, agents, "removed a relay host");
        self.emit(Event::RelayHostRemoved {
            host_id: host.to_string(),
        });
        Ok(())
    }

    async fn serve_channel(self: Arc<Self>, channel: IncomingChannel) {
        let IncomingChannel {
            channel: id,
            peer,
            stream,
        } = channel;
        let row = match self.store.peer_by_key(&peer.pubkey) {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(err) => {
                tracing::error!(error = %err, "cannot read a peer");
                return;
            }
        };
        if !self.relay.vouch(id, &peer.pubkey) {
            tracing::debug!(peer = %row.label, "the relay kept the limits of the address");
        }
        let (session, kill) = self.add_session(row.id);
        if let Err(err) = self.store.touch_peer(row.id) {
            tracing::warn!(error = %err, "cannot note when a peer was seen");
        }
        tracing::debug!(peer = %row.label, role = %row.role, "control connection opened");
        self.device_changed(row.id);
        let peer_id = row.id;
        let role = row.role;
        let calls = (role == Role::Host).then(|| self.attach_host(peer_id, session));
        Session::new(self.clone(), row, kill, calls)
            .run(stream)
            .await;
        if role == Role::Host {
            self.detach_host(peer_id, session);
        }
        self.remove_session(peer_id, session);
        self.device_changed(peer_id);
    }
}

impl Responder for Oracle {
    fn authorize(&self, client: &[u8; 32]) -> Result<String, String> {
        match self.store.peer_by_key(client) {
            Ok(Some(peer)) => Ok(peer.label),
            Ok(None) => Err("this key is not paired with the oracle; pair it first".into()),
            Err(err) => {
                tracing::error!(error = %err, "cannot read the peers");
                Err("the oracle cannot read its peers".into())
            }
        }
    }

    fn take_pairing(&self, id: &str) -> Option<Zeroizing<[u8; 32]>> {
        match self.store.take_pairing(id) {
            Ok(Some((row, secret))) => {
                let mut grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
                grants.retain(|_, (at, _)| at.elapsed() < GRANT_TIMEOUT);
                grants.insert(id.to_owned(), (Instant::now(), row));
                drop(grants);
                self.emit(Event::PairingRemoved {
                    id: id.to_owned(),
                    used: true,
                });
                Some(secret)
            }
            Ok(None) => None,
            Err(err) => {
                tracing::error!(error = %err, "cannot read the pairings");
                None
            }
        }
    }

    fn paired(&self, _client: [u8; 32], _label: &str) -> Result<(), String> {
        Err("the oracle pairs only with a pairing code".into())
    }

    fn paired_with(&self, client: [u8; 32], label: &str, pairing_id: &str) -> Result<(), String> {
        let grant = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(pairing_id);
        let Some((_, grant)) = grant else {
            return Err("the pairing code is unknown or expired".into());
        };
        let label = match grant.role {
            Role::Codespace => self.check_codespace_pairing(grant.target.as_deref())?,
            _ => {
                validate_label(label).map_err(|e| e.to_string())?;
                label.to_owned()
            }
        };
        let label = label.as_str();
        let (peer, replaced) = self
            .store
            .add_peer(grant.role, label, &client, Some(pairing_id))
            .map_err(|err| {
                tracing::error!(error = %err, "cannot save a pairing");
                "the oracle cannot save the pairing".to_owned()
            })?;
        let mut keys_changed = grant.role == Role::Device;
        for old in replaced {
            self.close_sessions(old.id);
            if old.role == Role::Device {
                keys_changed = true;
                if old.id != peer.id {
                    self.emit(Event::DeviceRemoved { id: old.id });
                }
            }
        }
        self.audit(
            Some(peer.id),
            "peer_paired",
            &format!("{} {label} with pairing {pairing_id}", peer.role),
        );
        tracing::info!(role = %peer.role, %label, "paired a peer");
        match peer.role {
            Role::Codespace => self.codespace_paired(label, peer.id),
            Role::Host => self.host_changed(peer.id),
            _ => self.device_changed(peer.id),
        }
        if keys_changed {
            self.emit(Event::DeviceKeysChanged);
        }
        Ok(())
    }
}

async fn run_control_agent(oracle: Arc<Oracle>, shutdown: CancellationToken) {
    while !shutdown.is_cancelled() {
        match oracle.relay.attach_local(oracle.agent_id()) {
            Ok(local) => {
                let (tx, mut rx) = mpsc::channel::<IncomingChannel>(64);
                let accepting = oracle.clone();
                tokio::spawn(async move {
                    while let Some(channel) = rx.recv().await {
                        tokio::spawn(accepting.clone().serve_channel(channel));
                    }
                });
                let responder: Arc<dyn Responder> = oracle.clone();
                let result = serve_local_link(
                    LocalLink {
                        tx: local.tx,
                        rx: local.rx,
                    },
                    oracle.identity.clone(),
                    responder,
                    tx,
                    shutdown.clone(),
                )
                .await;
                match result {
                    Ok(()) => break,
                    Err(err) => {
                        tracing::warn!(error = %err, "the control service lost its relay link")
                    }
                }
            }
            Err((_, message)) => {
                tracing::warn!(%message, "the control service cannot register with the relay")
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(RELINK_DELAY) => {}
        }
    }
}
