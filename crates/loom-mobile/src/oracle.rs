use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use loom_client::oracle_proto::{
    self as oracle, AgentAddress, CodespaceState, Event, FirstMessage, Repository,
};
use loom_client::proto::{InstanceId, PushRepositories, RepositoryStatus};
use loom_client::{
    ClientError, ClientIdentity, NewCodespace, OracleClient, OracleEndpoint, OracleEvents,
    RelayHost,
};
use loom_view_model::codespaces::{
    self, can_stop, directories, format_memory, new_codespace_defaults, parse_memory, session_info,
    state_label, work_text,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::agent::{AgentConnection, with_client};
use crate::error::{LoomError, Result};
use crate::hosts::CLIENT_NAME;
use crate::records::SessionSummary;
use crate::runtime::{run, spawn};

const KEYS_FILE: &str = "codespaces.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CodespaceStatus {
    Provisioning,
    Starting,
    Ready,
    Closing,
    Closed,
    Failed,
    Lost,
    Stopped,
    Unknown,
}

impl From<CodespaceState> for CodespaceStatus {
    fn from(state: CodespaceState) -> Self {
        match state {
            CodespaceState::Provisioning => Self::Provisioning,
            CodespaceState::Starting => Self::Starting,
            CodespaceState::Ready => Self::Ready,
            CodespaceState::Closing => Self::Closing,
            CodespaceState::Closed => Self::Closed,
            CodespaceState::Failed => Self::Failed,
            CodespaceState::Lost => Self::Lost,
            CodespaceState::Stopped => Self::Stopped,
            CodespaceState::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OracleRepository {
    pub url: String,
    pub directory: String,
    pub base_branch: Option<String>,
}

impl From<&Repository> for OracleRepository {
    fn from(repository: &Repository) -> Self {
        Self {
            url: repository.url.clone(),
            directory: codespaces::directory(repository),
            base_branch: repository.base_branch.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OracleWorkspace {
    pub name: String,
    pub host: String,
    pub repositories: Vec<OracleRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Codespace {
    pub id: String,
    pub workspace: String,
    pub host: String,
    pub preset: Option<String>,
    pub status: CodespaceStatus,
    pub state: String,
    pub branch: String,
    pub cpus: u32,
    pub memory: String,
    pub step: Option<String>,
    pub error: Option<String>,
    pub sessions: u32,
    pub instance: Option<String>,
    pub repositories: Vec<String>,
    pub created_at: u64,
    pub stoppable: bool,
    pub stop_at: Option<u64>,
    pub delete_at: Option<u64>,
}

impl Codespace {
    fn of(codespace: &oracle::Codespace, sessions: u32) -> Self {
        Self {
            id: codespace.id.clone(),
            workspace: codespace.workspace.clone(),
            host: codespace.host.clone(),
            preset: codespace.preset.clone(),
            status: codespace.state.into(),
            state: state_label(codespace),
            branch: codespace.branch.clone(),
            cpus: codespace.cpus,
            memory: format_memory(codespace.memory_mib),
            step: codespace.step.clone(),
            error: codespace.error.clone(),
            sessions,
            instance: codespace.agent.as_ref().and_then(|a| a.instance.clone()),
            repositories: directories(&codespace.repositories),
            created_at: codespace.created_at,
            stoppable: can_stop(codespace),
            stop_at: codespace.stop_at,
            delete_at: codespace.delete_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CodespaceSession {
    pub codespace: String,
    pub workspace: String,
    pub branch: String,
    pub instance: Option<String>,
    pub session: SessionSummary,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OraclePreset {
    pub name: String,
    pub model_accounts: Vec<String>,
}

impl From<&oracle::Preset> for OraclePreset {
    fn from(preset: &oracle::Preset) -> Self {
        Self {
            name: preset.name.clone(),
            model_accounts: preset
                .model_accounts
                .iter()
                .map(|(provider, account)| format!("{provider}: {account}"))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OracleLists {
    pub connected: bool,
    pub error: Option<String>,
    pub workspaces: Vec<OracleWorkspace>,
    pub codespaces: Vec<Codespace>,
    pub sessions: Vec<CodespaceSession>,
    pub presets: Vec<OraclePreset>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NewCodespaceForm {
    pub workspace: String,
    pub host: String,
    pub host_detail: Option<String>,
    pub presets: Vec<OraclePreset>,
    pub preset: Option<String>,
    pub cpus: u32,
    pub memory: String,
    pub repositories: Vec<OracleRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BaseBranch {
    pub directory: String,
    pub branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NewCodespaceRequest {
    pub workspace: String,
    pub preset: Option<String>,
    pub cpus: u32,
    pub memory: String,
    pub base_branches: Vec<BaseBranch>,
    pub first_message: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OracleModel {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OracleModels {
    pub models: Vec<OracleModel>,
    pub default_model: Option<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CreatedCodespace {
    pub codespace: Codespace,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RepositoryState {
    pub directory: String,
    pub branch: String,
    pub base_branch: Option<String>,
    pub changed_files: u32,
    pub ahead: u32,
    pub unpushed: u32,
    pub pull_request: Option<String>,
    pub error: Option<String>,
    pub needs_choice: bool,
    pub detail: String,
}

impl From<&RepositoryStatus> for RepositoryState {
    fn from(status: &RepositoryStatus) -> Self {
        Self {
            directory: status.directory.clone(),
            branch: status.branch.clone(),
            base_branch: status.base_branch.clone(),
            changed_files: status.changed_files,
            ahead: status.ahead,
            unpushed: status.unpushed,
            pull_request: status.pull_request.clone(),
            error: status.error.clone(),
            needs_choice: loom_client::needs_choice(status),
            detail: work_text(status),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PullRequestOutcome {
    pub directory: String,
    pub text: String,
    pub url: Option<String>,
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CodespaceKey {
    pub oracle: String,
    pub codespace: String,
    pub workspace: String,
    pub branch: String,
    pub host_id: String,
    pub host_key: String,
    pub instance: Option<String>,
    #[serde(default)]
    pub repositories: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct KeyBook {
    path: PathBuf,
    keys: Mutex<Vec<CodespaceKey>>,
}

impl KeyBook {
    pub(crate) fn open(dir: &Path) -> Self {
        let path = dir.join(KEYS_FILE);
        let keys = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            keys: Mutex::new(keys),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<CodespaceKey>> {
        self.keys.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn find(&self, host_id: &str) -> Option<CodespaceKey> {
        self.lock()
            .iter()
            .find(|k| k.host_id.eq_ignore_ascii_case(host_id.trim()))
            .cloned()
    }

    pub(crate) fn of_instance(&self, oracle: &str, instance: &str) -> Option<CodespaceKey> {
        self.lock()
            .iter()
            .find(|k| k.oracle == oracle && k.instance.as_deref() == Some(instance))
            .cloned()
    }

    pub(crate) fn set(&self, oracle: &str, keys: Vec<CodespaceKey>) {
        let mut all = self.lock();
        let mut next: Vec<CodespaceKey> =
            all.iter().filter(|k| k.oracle != oracle).cloned().collect();
        next.extend(keys);
        if *all == next {
            return;
        }
        *all = next;
        if let Err(err) = self.save(&all) {
            tracing::warn!(error = %err, "cannot save the keys of the codespaces");
        }
    }

    pub(crate) fn rename(&self, from: &str, to: &str) {
        let mut all = self.lock();
        let mut changed = false;
        for key in all.iter_mut().filter(|k| k.oracle == from) {
            key.oracle = to.to_owned();
            changed = true;
        }
        if changed && let Err(err) = self.save(&all) {
            tracing::warn!(error = %err, "cannot save the keys of the codespaces");
        }
    }

    fn save(&self, keys: &[CodespaceKey]) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(keys)?)?;
        std::fs::rename(&tmp, &self.path)
    }
}

#[derive(Default)]
struct Lists {
    workspaces: Vec<oracle::Workspace>,
    codespaces: Vec<oracle::Codespace>,
    presets: Vec<oracle::Preset>,
    hosts: Vec<oracle::Host>,
    sessions: Vec<oracle::CodespaceSession>,
}

struct State {
    client: Option<OracleClient>,
    error: Option<String>,
    lists: Lists,
    progress: HashMap<String, Vec<String>>,
    changed: bool,
    closed: bool,
    epoch: u64,
}

struct Shared {
    name: String,
    endpoint: OracleEndpoint,
    identity: Arc<ClientIdentity>,
    keys: Arc<KeyBook>,
    state: Mutex<State>,
    wake: Notify,
}

#[derive(uniffi::Object)]
pub struct OracleConnection {
    shared: Arc<Shared>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn client(&self) -> Result<OracleClient> {
        self.lock()
            .client
            .clone()
            .filter(|c| !c.is_closed())
            .ok_or(LoomError::Closed)
    }

    async fn connect(self: &Arc<Self>) -> Result<()> {
        let (client, events) = OracleClient::connect(&self.endpoint, &self.identity, CLIENT_NAME)
            .await
            .map_err(oracle_error)?;
        client.subscribe().await.map_err(oracle_error)?;
        let lists = load(&client).await?;
        let epoch = {
            let mut state = self.lock();
            state.epoch += 1;
            state.client = Some(client);
            state.error = None;
            state.lists = lists;
            state.changed = true;
            state.epoch
        };
        self.save_keys();
        self.wake.notify_one();
        let shared = self.clone();
        spawn(async move { shared.follow(events, epoch).await });
        Ok(())
    }

    async fn follow(self: Arc<Self>, mut events: OracleEvents, epoch: u64) {
        while let Some(event) = events.recv().await {
            if self.lock().epoch != epoch {
                return;
            }
            if matches!(event, Event::EventsLost) {
                let Ok(client) = self.client() else {
                    return;
                };
                match load(&client).await {
                    Ok(lists) => self.lock().lists = lists,
                    Err(err) => tracing::warn!(error = %err, "cannot read the lists of the oracle"),
                }
            } else {
                self.apply(event);
            }
            self.lock().changed = true;
            self.save_keys();
            self.wake.notify_one();
        }
        let mut state = self.lock();
        if state.epoch == epoch && !state.closed {
            state.client = None;
            state.error = Some("The connection to the oracle closed.".into());
            state.changed = true;
        }
        drop(state);
        self.wake.notify_one();
    }

    fn apply(&self, event: Event) {
        let mut state = self.lock();
        let state = &mut *state;
        let lists = &mut state.lists;
        match event {
            Event::WorkspaceChanged { workspace } => {
                upsert(&mut lists.workspaces, workspace, |a, b| a.name == b.name)
            }
            Event::WorkspaceRemoved { name } => lists.workspaces.retain(|w| w.name != name),
            Event::CodespaceChanged { codespace } => {
                upsert(&mut lists.codespaces, codespace, |a, b| a.id == b.id)
            }
            Event::CodespaceProgress { id, step } => {
                if let Some(codespace) = lists.codespaces.iter_mut().find(|c| c.id == id) {
                    codespace.step = Some(step.clone());
                }
                state.progress.entry(id).or_default().push(step);
            }
            Event::CodespaceRemoved { id } => {
                lists.codespaces.retain(|c| c.id != id);
                lists.sessions.retain(|s| s.codespace != id);
                state.progress.remove(&id);
            }
            Event::PresetChanged { preset } => {
                upsert(&mut lists.presets, preset, |a, b| a.name == b.name)
            }
            Event::PresetRemoved { name } => lists.presets.retain(|p| p.name != name),
            Event::HostChanged { host } => upsert(&mut lists.hosts, host, |a, b| a.name == b.name),
            Event::HostRemoved { name } => lists.hosts.retain(|h| h.name != name),
            Event::SessionChanged { session } => upsert(&mut lists.sessions, session, |a, b| {
                a.codespace == b.codespace && a.session.id == b.session.id
            }),
            Event::SessionRemoved { codespace, id } => lists
                .sessions
                .retain(|s| !(s.codespace == codespace && s.session.id == id)),
            _ => {}
        }
    }

    fn save_keys(&self) {
        let keys = {
            let state = self.lock();
            state
                .lists
                .codespaces
                .iter()
                .filter(|c| c.state != CodespaceState::Closed)
                .filter_map(|c| {
                    let agent = c.agent.as_ref()?;
                    Some(CodespaceKey {
                        oracle: self.name.clone(),
                        codespace: c.id.clone(),
                        workspace: c.workspace.clone(),
                        branch: c.branch.clone(),
                        host_id: agent.host_id.clone(),
                        host_key: agent.host_key.clone(),
                        instance: agent.instance.clone(),
                        repositories: directories(&c.repositories),
                    })
                })
                .collect()
        };
        self.keys.set(&self.name, keys);
    }

    fn address(&self, id: &str) -> Result<(oracle::Codespace, AgentAddress)> {
        let state = self.lock();
        let codespace = state
            .lists
            .codespaces
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| LoomError::invalid(format!("the oracle has no codespace {id}")))?;
        let address = codespace
            .agent
            .clone()
            .filter(|_| codespace.state == CodespaceState::Ready)
            .ok_or_else(|| LoomError::Offline {
                message: format!(
                    "The codespace is {}. Its agent is not ready.",
                    codespace.state
                ),
            })?;
        Ok((codespace, address))
    }

    fn lists(&self) -> OracleLists {
        let state = self.lock();
        let lists = &state.lists;
        let count = |id: &str| {
            lists
                .sessions
                .iter()
                .filter(|s| s.codespace == id && !s.session.archived)
                .count() as u32
        };
        let mut codespaces: Vec<Codespace> = lists
            .codespaces
            .iter()
            .filter(|c| codespaces::is_listed(c))
            .map(|c| Codespace::of(c, count(&c.id)))
            .collect();
        codespaces.sort_by_key(|c| std::cmp::Reverse(c.created_at));
        let mut workspaces: Vec<OracleWorkspace> = lists
            .workspaces
            .iter()
            .map(|w| OracleWorkspace {
                name: w.name.clone(),
                host: w.host.clone(),
                repositories: w.repositories.iter().map(OracleRepository::from).collect(),
            })
            .collect();
        workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        let sessions = lists
            .sessions
            .iter()
            .filter_map(|s| {
                let codespace = lists.codespaces.iter().find(|c| c.id == s.codespace)?;
                if !codespaces::is_listed(codespace) {
                    return None;
                }
                Some(CodespaceSession {
                    codespace: s.codespace.clone(),
                    workspace: s.workspace.clone(),
                    branch: codespace.branch.clone(),
                    instance: codespace.agent.as_ref().and_then(|a| a.instance.clone()),
                    session: SessionSummary::from(&session_info(&s.session)),
                    updated_at: s.session.updated_at,
                })
            })
            .collect();
        let mut presets: Vec<OraclePreset> = lists.presets.iter().map(OraclePreset::from).collect();
        presets.sort_by(|a, b| a.name.cmp(&b.name));
        OracleLists {
            connected: state.client.as_ref().is_some_and(|c| !c.is_closed()),
            error: state.error.clone(),
            workspaces,
            codespaces,
            sessions,
            presets,
        }
    }
}

fn upsert<T>(list: &mut Vec<T>, item: T, same: impl Fn(&T, &T) -> bool) {
    match list.iter_mut().find(|known| same(known, &item)) {
        Some(known) => *known = item,
        None => list.push(item),
    }
}

fn oracle_error(err: ClientError) -> LoomError {
    match err {
        ClientError::Oracle(err) => LoomError::Api {
            code: format!("{:?}", err.code),
            message: err.message,
        },
        err => err.into(),
    }
}

async fn load(client: &OracleClient) -> Result<Lists> {
    Ok(Lists {
        workspaces: client.workspaces().await.map_err(oracle_error)?,
        codespaces: client.codespaces().await.map_err(oracle_error)?,
        presets: client.presets().await.map_err(oracle_error)?,
        hosts: client.hosts().await.map_err(oracle_error)?,
        sessions: client.sessions(None).await.map_err(oracle_error)?,
    })
}

impl OracleConnection {
    pub(crate) async fn open(
        name: String,
        endpoint: OracleEndpoint,
        identity: Arc<ClientIdentity>,
        keys: Arc<KeyBook>,
    ) -> Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            name,
            endpoint,
            identity,
            keys,
            state: Mutex::new(State {
                client: None,
                error: None,
                lists: Lists::default(),
                progress: HashMap::new(),
                changed: false,
                closed: false,
                epoch: 0,
            }),
            wake: Notify::new(),
        });
        let opened = shared.clone();
        run(async move { opened.connect().await }).await?;
        Ok(Arc::new(Self { shared }))
    }
}

#[uniffi::export]
impl OracleConnection {
    pub fn name(&self) -> String {
        self.shared.name.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.client().is_ok()
    }

    pub fn lists(&self) -> OracleLists {
        self.shared.lists()
    }

    pub async fn next_lists(self: Arc<Self>) -> Option<OracleLists> {
        let shared = self.shared.clone();
        run(async move {
            loop {
                {
                    let mut state = shared.lock();
                    if state.closed {
                        return None;
                    }
                    if std::mem::take(&mut state.changed) {
                        drop(state);
                        return Some(shared.lists());
                    }
                }
                shared.wake.notified().await;
            }
        })
        .await
    }

    pub async fn reconnect(self: Arc<Self>) -> Result<()> {
        let shared = self.shared.clone();
        run(async move {
            if shared.lock().closed {
                return Err(LoomError::Closed);
            }
            match shared.connect().await {
                Ok(()) => Ok(()),
                Err(err) => {
                    let mut state = shared.lock();
                    state.error = Some(err.to_string());
                    state.changed = true;
                    drop(state);
                    shared.wake.notify_one();
                    Err(err)
                }
            }
        })
        .await
    }

    pub fn disconnect(&self) {
        let mut state = self.shared.lock();
        state.closed = true;
        state.client = None;
        state.epoch += 1;
        drop(state);
        self.shared.wake.notify_one();
    }

    pub fn progress(&self, codespace: String) -> Vec<String> {
        self.shared
            .lock()
            .progress
            .get(&codespace)
            .cloned()
            .unwrap_or_default()
    }

    pub fn codespace(&self, id: String) -> Option<Codespace> {
        self.shared
            .lists()
            .codespaces
            .into_iter()
            .find(|c| c.id == id)
    }

    pub fn new_codespace_form(&self, workspace: String) -> Result<NewCodespaceForm> {
        let state = self.shared.lock();
        let lists = &state.lists;
        let found = lists
            .workspaces
            .iter()
            .find(|w| w.name == workspace)
            .ok_or_else(|| {
                LoomError::invalid(format!("the oracle has no workspace {workspace}"))
            })?;
        let defaults = new_codespace_defaults(&lists.codespaces, &workspace);
        let mut presets: Vec<OraclePreset> = lists.presets.iter().map(OraclePreset::from).collect();
        presets.sort_by(|a, b| a.name.cmp(&b.name));
        let preset = defaults
            .preset
            .filter(|name| presets.iter().any(|p| &p.name == name))
            .or_else(|| presets.first().map(|p| p.name.clone()));
        let host_detail = lists
            .hosts
            .iter()
            .find(|h| h.name == found.host)
            .map(|host| {
                let state = if host.connected {
                    "connected"
                } else {
                    "not connected"
                };
                match &host.report {
                    Some(report) => format!(
                        "{state}, {} CPUs, {}",
                        report.cpus,
                        format_memory(report.memory_mib)
                    ),
                    None => state.to_owned(),
                }
            });
        Ok(NewCodespaceForm {
            workspace: found.name.clone(),
            host: found.host.clone(),
            host_detail,
            presets,
            preset,
            cpus: defaults.cpus,
            memory: format_memory(defaults.memory_mib),
            repositories: found
                .repositories
                .iter()
                .map(OracleRepository::from)
                .collect(),
        })
    }

    pub async fn codespace_warnings(
        self: Arc<Self>,
        workspace: String,
        preset: Option<String>,
    ) -> Result<Vec<String>> {
        let client = self.shared.client()?;
        run(async move {
            client
                .codespace_warnings(&workspace, preset.as_deref())
                .await
                .map_err(oracle_error)
        })
        .await
    }

    pub async fn models(self: Arc<Self>, preset: String) -> Result<OracleModels> {
        let client = self.shared.client()?;
        run(async move {
            let models = client.models(&preset).await.map_err(oracle_error)?;
            Ok(OracleModels {
                models: models
                    .models
                    .iter()
                    .map(|model| {
                        let name = model
                            .display_name
                            .clone()
                            .unwrap_or_else(|| model.id.clone());
                        let label = match &model.account {
                            Some(account) => format!("{name} ({}, {account})", model.provider),
                            None => format!("{name} ({})", model.provider),
                        };
                        OracleModel {
                            id: model.id.clone(),
                            label,
                        }
                    })
                    .collect(),
                default_model: models.default_model,
                errors: models.errors,
            })
        })
        .await
    }

    pub async fn create_codespace(
        self: Arc<Self>,
        request: NewCodespaceRequest,
    ) -> Result<CreatedCodespace> {
        let memory_mib = parse_memory(&request.memory).ok_or_else(|| {
            LoomError::invalid(format!(
                "{:?} is not an amount of memory, such as 4 GiB or 512 MiB",
                request.memory
            ))
        })?;
        if request.cpus == 0 {
            return Err(LoomError::invalid("a codespace needs one CPU or more"));
        }
        let base_branches: BTreeMap<String, String> = request
            .base_branches
            .into_iter()
            .map(|b| (b.directory, b.branch.trim().to_owned()))
            .filter(|(_, branch)| !branch.is_empty())
            .collect();
        let first_message = request
            .first_message
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .map(|text| FirstMessage {
                text,
                model: request.model,
            });
        let new = NewCodespace {
            workspace: request.workspace,
            preset: request.preset,
            cpus: Some(request.cpus),
            memory_mib: Some(memory_mib),
            base_branch: None,
            base_branches,
            client_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            first_message,
        };
        let client = self.shared.client()?;
        let shared = self.shared.clone();
        run(async move {
            let created = client
                .create_codespace_with_warnings(new)
                .await
                .map_err(oracle_error)?;
            let mut state = shared.lock();
            upsert(
                &mut state.lists.codespaces,
                created.codespace.clone(),
                |a, b| a.id == b.id,
            );
            state.changed = true;
            drop(state);
            shared.wake.notify_one();
            Ok(CreatedCodespace {
                codespace: Codespace::of(&created.codespace, 0),
                warnings: created.warnings,
            })
        })
        .await
    }

    pub async fn close_codespace(self: Arc<Self>, id: String) -> Result<Codespace> {
        let client = self.shared.client()?;
        run(async move {
            let codespace = client.close_codespace(&id).await.map_err(oracle_error)?;
            Ok(Codespace::of(&codespace, 0))
        })
        .await
    }

    pub async fn stop_codespace(self: Arc<Self>, id: String) -> Result<Codespace> {
        let client = self.shared.client()?;
        run(async move {
            let codespace = client.stop_codespace(&id).await.map_err(oracle_error)?;
            Ok(Codespace::of(&codespace, 0))
        })
        .await
    }

    pub async fn resume_codespace(self: Arc<Self>, id: String) -> Result<Codespace> {
        let client = self.shared.client()?;
        run(async move {
            let codespace = client.resume_codespace(&id).await.map_err(oracle_error)?;
            Ok(Codespace::of(&codespace, 0))
        })
        .await
    }

    pub async fn connect_codespace(self: Arc<Self>, id: String) -> Result<Arc<AgentConnection>> {
        let (codespace, address) = self.shared.address(&id)?;
        let relay = RelayHost::from_parts(
            &self.shared.endpoint.url,
            &address.host_id,
            &address.host_key,
            self.shared.endpoint.access.clone(),
        )
        .map_err(|err| LoomError::invalid(err.to_string()))?;
        let instance = address.instance.ok_or_else(|| LoomError::Offline {
            message: "The agent of the codespace has no instance.".into(),
        })?;
        let connection =
            AgentConnection::open(relay, InstanceId(instance), self.shared.identity.clone())
                .await?;
        connection.set_directories(directories(&codespace.repositories));
        Ok(connection)
    }

    pub async fn set_push_token(self: Arc<Self>, token: Option<String>) -> Result<()> {
        let client = self.shared.client()?;
        run(async move {
            client
                .set_push_token(token.as_deref())
                .await
                .map_err(oracle_error)
        })
        .await
    }
}

#[uniffi::export]
impl AgentConnection {
    pub async fn repositories(self: Arc<Self>) -> Result<Vec<RepositoryState>> {
        with_client(&self.shared, move |client| async move {
            Ok(client
                .repositories()
                .await?
                .iter()
                .map(RepositoryState::from)
                .collect())
        })
        .await
    }

    pub async fn push_repositories(
        self: Arc<Self>,
        directories: Vec<String>,
        message: Option<String>,
    ) -> Result<Vec<RepositoryState>> {
        with_client(&self.shared, move |client| async move {
            let statuses = client
                .push_repositories(PushRepositories {
                    commit_message: message.filter(|m| !m.trim().is_empty()),
                    directories,
                })
                .await?;
            Ok(statuses.iter().map(RepositoryState::from).collect())
        })
        .await
    }

    pub async fn open_pull_requests(
        self: Arc<Self>,
        title: String,
        body: String,
        draft: bool,
    ) -> Result<Vec<PullRequestOutcome>> {
        with_client(&self.shared, move |client| async move {
            let results = client.open_pull_requests(&title, &body, draft).await?;
            Ok(results
                .into_iter()
                .map(|result| {
                    let text = match (&result.url, &result.skipped, &result.error) {
                        (_, _, Some(error)) => format!("error: {error}"),
                        (Some(url), _, None) if result.created => format!("opened {url}"),
                        (Some(url), _, None) => format!("updated {url}"),
                        (None, Some(reason), None) => format!("skipped: {reason}"),
                        (None, None, None) => "nothing to do".to_owned(),
                    };
                    PullRequestOutcome {
                        directory: result.directory,
                        failed: result.error.is_some(),
                        url: result.url,
                        text,
                    }
                })
                .collect())
        })
        .await
    }
}

#[uniffi::export]
pub fn in_time(now: u64, then: u64) -> String {
    loom_view_model::settings::in_time(now, then)
}

#[uniffi::export]
pub fn close_plan(
    repositories: Vec<RepositoryState>,
    push: Vec<String>,
    discard: Vec<String>,
    message: Option<String>,
) -> Result<Vec<String>> {
    let statuses: Vec<RepositoryStatus> = repositories
        .into_iter()
        .map(|r| RepositoryStatus {
            directory: r.directory,
            path: String::new(),
            url: String::new(),
            provider: None,
            branch: r.branch,
            base_branch: r.base_branch,
            changed_files: r.changed_files,
            ahead: r.ahead,
            unpushed: r.unpushed,
            pushed: false,
            pull_request: r.pull_request,
            error: r.error,
        })
        .collect();
    let choose = loom_client::choices_from_lists(&statuses, &push, &discard)
        .map_err(|err| LoomError::invalid(err.to_string()))?;
    let has_message = message.is_some_and(|m| !m.trim().is_empty());
    loom_client::close_plan(&statuses, choose, has_message)
        .map_err(|err| LoomError::invalid(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(directory: &str, changed_files: u32, unpushed: u32) -> RepositoryState {
        let status = RepositoryStatus {
            directory: directory.into(),
            path: String::new(),
            url: String::new(),
            provider: None,
            branch: "loom/c1".into(),
            base_branch: Some("main".into()),
            changed_files,
            ahead: unpushed,
            unpushed,
            pushed: false,
            pull_request: None,
            error: None,
        };
        RepositoryState::from(&status)
    }

    #[test]
    fn the_close_plan_needs_a_choice_for_each_repository_with_work() {
        let repos = vec![state("app", 2, 0), state("lib", 0, 1), state("docs", 0, 0)];
        assert!(repos[0].needs_choice && repos[1].needs_choice && !repos[2].needs_choice);
        assert_eq!(repos[0].detail, "2 changed files");
        assert!(close_plan(repos.clone(), vec![], vec!["app".into()], None).is_err());
        assert!(close_plan(repos.clone(), vec!["app".into()], vec!["lib".into()], None).is_err());
        assert_eq!(
            close_plan(
                repos.clone(),
                vec!["app".into(), "lib".into()],
                vec![],
                Some("save".into())
            )
            .unwrap(),
            ["app", "lib"]
        );
        assert_eq!(
            close_plan(repos, vec![], vec!["*".into()], None).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_keys_of_the_codespaces_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let book = KeyBook::open(dir.path());
        let key = |oracle: &str, id: &str| CodespaceKey {
            oracle: oracle.into(),
            codespace: id.into(),
            workspace: "acme".into(),
            branch: format!("loom/{id}"),
            host_id: format!("{id:0>64}"),
            host_key: "k".into(),
            instance: Some(format!("i-{id}")),
            repositories: Vec::new(),
        };
        book.set("next", vec![key("next", "a"), key("next", "b")]);
        book.set("other", vec![key("other", "c")]);
        book.set("next", vec![key("next", "b")]);
        let again = KeyBook::open(dir.path());
        assert!(again.find(&format!("{:0>64}", "a")).is_none());
        assert_eq!(again.find(&format!("{:0>64}", "b")).unwrap().oracle, "next");
        again.rename("other", "lab");
        assert_eq!(
            KeyBook::open(dir.path())
                .find(&format!("{:0>64}", "c"))
                .unwrap()
                .oracle,
            "lab"
        );
    }
}
