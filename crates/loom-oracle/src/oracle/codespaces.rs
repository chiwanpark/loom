use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use loom_crypto::decode_key;
use loom_oracle_proto::{
    AgentAddress, Bootstrap, Bundle, CONNECTION_RELAY, Codespace, CodespaceState, CreateInstance,
    DEFAULT_CPUS, DEFAULT_MEMORY_MIB, DeviceKey, Environment, ErrorCode, Event, FirstMessage, Host,
    HostCall, HostReport, InstanceReport, OracleError, Reply, Repository, Role, ServerMessage,
    Workspace, WorkspaceVolume, valid_name,
};
use loom_proto::relay::HostId;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::{Oracle, bad_request, not_found, pairing_link};
use crate::store::{CodespaceRow, HostRow, WorkspaceRow, now_unix};

#[derive(Debug, Clone, Default)]
pub struct NewCodespaceArgs {
    pub workspace: String,
    pub preset: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub base_branch: Option<String>,
    pub base_branches: BTreeMap<String, String>,
    pub client_version: Option<String>,
    pub first_message: Option<FirstMessage>,
}

const BOOTSTRAP_TTL: Duration = Duration::from_secs(6 * 3600);
const CREATE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const DELETE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CALL_QUEUE: usize = 32;
const LOST_GRACE_SECS: u64 = 60;
const MIN_MEMORY_MIB: u64 = 256;
const MAX_SCRIPT: usize = 256 * 1024;
const MAX_VALUE: usize = 4096;

pub(super) struct HostLink {
    session: u64,
    tx: mpsc::Sender<ServerMessage>,
}

pub(super) struct PendingCall {
    host: u64,
    tx: oneshot::Sender<Result<Value, OracleError>>,
}

fn unavailable(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Unavailable, message)
}

fn conflict(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Conflict, message)
}

fn forbidden(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Forbidden, message)
}

fn new_codespace_id() -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 8];
    rand::fill(&mut bytes);
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let set = if i == 0 { LETTERS } else { CHARS };
            set[usize::from(*b) % set.len()] as char
        })
        .collect()
}

pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

fn valid_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() <= 500
        && url.len() > "https://".len()
        && url.chars().all(|c| c.is_ascii_graphic())
}

fn valid_variable(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

pub fn validate_environment(environment: &Environment) -> Result<(), OracleError> {
    if let Some(image) = &environment.base_image
        && (image.is_empty() || image.len() > 200 || !image.chars().all(|c| c.is_ascii_graphic()))
    {
        return Err(bad_request(format!("{image:?} is not an image name")));
    }
    if environment
        .setup_script
        .as_ref()
        .is_some_and(|script| script.len() > MAX_SCRIPT)
    {
        return Err(bad_request(format!(
            "the setup script is longer than {MAX_SCRIPT} bytes"
        )));
    }
    for (name, value) in &environment.variables {
        if !valid_variable(name) {
            return Err(bad_request(format!(
                "{name:?} is not a name of an environment variable"
            )));
        }
        if value.len() > MAX_VALUE || value.contains(['\0', '\n']) {
            return Err(bad_request(format!(
                "the value of {name} must be one line of up to {MAX_VALUE} bytes"
            )));
        }
    }
    let certificates: usize = environment.ca_certificates.iter().map(String::len).sum();
    if certificates > MAX_SCRIPT
        || environment
            .ca_certificates
            .iter()
            .any(|pem| !pem.contains("-----BEGIN CERTIFICATE-----"))
    {
        return Err(bad_request(
            "each CA certificate must be in PEM form, and all of them up to 256 KiB",
        ));
    }
    if let Some(mirror) = environment
        .docker
        .registry_mirrors
        .iter()
        .find(|mirror| !valid_url(mirror))
    {
        return Err(bad_request(format!(
            "{mirror:?} is not an HTTP or HTTPS URL"
        )));
    }
    Ok(())
}

fn workspace(row: &WorkspaceRow) -> Workspace {
    Workspace {
        name: row.name.clone(),
        host: row.host_name.clone(),
        repositories: row.repositories.clone(),
        volumes: row.volumes.clone(),
        created_at: row.created_at,
    }
}

pub(super) fn codespace(row: &CodespaceRow) -> Codespace {
    Codespace {
        id: row.id.clone(),
        workspace: row.workspace_name.clone(),
        host: row.host_name.clone(),
        preset: row.preset_name.clone(),
        state: row.state,
        cpus: row.cpus,
        memory_mib: row.memory_mib,
        instance: row.instance.clone(),
        branch: row.branch.clone(),
        base_branch: row.base_branch.clone(),
        repositories: row.repositories.clone(),
        agent: row.agent.clone(),
        step: row.step.clone(),
        error: row.error.clone(),
        first_message: row.first_message.is_some(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub(super) fn admitted_agent(row: &CodespaceRow) -> Option<HostId> {
    let open = matches!(
        row.state,
        CodespaceState::Starting | CodespaceState::Ready | CodespaceState::Lost
    );
    open.then(|| row.agent.as_ref()?.host_id.parse().ok())
        .flatten()
}

impl Oracle {
    pub(super) fn lock_links(&self) -> std::sync::MutexGuard<'_, HashMap<u64, HostLink>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_calls(&self) -> std::sync::MutexGuard<'_, HashMap<u64, PendingCall>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn host_connected(&self, host: u64) -> bool {
        self.lock_links().contains_key(&host)
    }

    pub fn attach_host(self: &Arc<Self>, host: u64, session: u64) -> mpsc::Receiver<ServerMessage> {
        let (tx, rx) = mpsc::channel(CALL_QUEUE);
        self.lock_links().insert(host, HostLink { session, tx });
        self.host_changed(host);
        self.refresh_reach(host);
        rx
    }

    pub fn detach_host(&self, host: u64, session: u64) {
        let removed = {
            let mut links = self.lock_links();
            match links.get(&host) {
                Some(link) if link.session == session => links.remove(&host).is_some(),
                _ => false,
            }
        };
        if removed {
            self.lock_calls().retain(|_, call| call.host != host);
            self.host_changed(host);
        }
    }

    pub fn reply(&self, host: u64, reply: Reply) {
        let pending = {
            let mut calls = self.lock_calls();
            match calls.get(&reply.id) {
                Some(call) if call.host == host => calls.remove(&reply.id),
                _ => None,
            }
        };
        let Some(pending) = pending else {
            tracing::debug!(id = reply.id, "ignored a reply to an unknown call");
            return;
        };
        let result = match reply.error {
            Some(error) => Err(error),
            None => Ok(reply.result.unwrap_or(Value::Null)),
        };
        let _ = pending.tx.send(result);
    }

    pub(super) async fn call_host(
        &self,
        host: u64,
        call: HostCall,
        timeout: Duration,
    ) -> Result<Value, OracleError> {
        let link = self.lock_links().get(&host).map(|link| link.tx.clone());
        let Some(link) = link else {
            return Err(unavailable("the host is not connected"));
        };
        let id = self
            .next_call
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.lock_calls().insert(id, PendingCall { host, tx });
        if link.send(ServerMessage::Call { id, call }).await.is_err() {
            self.lock_calls().remove(&id);
            return Err(unavailable("the host is not connected"));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(unavailable("the host disconnected")),
            Err(_) => {
                self.lock_calls().remove(&id);
                Err(unavailable("the host did not answer in time"))
            }
        }
    }

    fn host(&self, row: &HostRow) -> Host {
        Host {
            id: row.peer.id,
            name: row.peer.label.clone(),
            added_at: row.peer.added_at,
            connected: self.host_connected(row.peer.id),
            report: row.report.clone(),
            reported_at: row.reported_at,
            environment: row.environment.clone(),
            codespaces: self.store.count_open_codespaces(row.peer.id).unwrap_or(0),
            reach: self.host_reach(row.peer.id),
        }
    }

    pub(super) fn host_changed(&self, peer: u64) {
        match self.store.host(peer) {
            Ok(Some(row)) => self.emit(Event::HostChanged {
                host: self.host(&row),
            }),
            Ok(None) => {}
            Err(err) => tracing::error!(error = %err, "cannot read a host"),
        }
    }

    pub fn hosts(&self) -> Result<Vec<Host>, OracleError> {
        Ok(self
            .store
            .hosts()?
            .iter()
            .map(|row| self.host(row))
            .collect())
    }

    fn host_by_name(&self, name: &str) -> Result<HostRow, OracleError> {
        self.store
            .host_by_name(name)?
            .ok_or_else(|| not_found(format!("no host has the name {name}")))
    }

    pub fn remove_host(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        let row = self.host_by_name(name)?;
        let closed = self.closed_codespaces(|c| c.host == row.peer.id)?;
        self.store
            .remove_host(row.peer.id)?
            .ok_or_else(|| not_found(format!("no host has the name {name}")))?;
        self.close_sessions(row.peer.id);
        self.audit(by, "host_removed", &format!("host {name}"));
        tracing::info!(host = %name, "removed a host");
        self.codespaces_removed(closed);
        self.emit(Event::HostRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    pub fn set_host_environment(
        &self,
        name: &str,
        environment: &Environment,
        by: Option<u64>,
    ) -> Result<Host, OracleError> {
        validate_environment(environment)?;
        let row = self.host_by_name(name)?;
        self.store.set_host_environment(row.peer.id, environment)?;
        self.audit(by, "host_environment_set", &format!("host {name}"));
        let row = self.host_by_name(name)?;
        let host = self.host(&row);
        self.emit(Event::HostChanged { host: host.clone() });
        Ok(host)
    }

    pub fn workspaces(&self) -> Result<Vec<Workspace>, OracleError> {
        Ok(self.store.workspaces()?.iter().map(workspace).collect())
    }

    async fn check_host(
        &self,
        host: &HostRow,
        repositories: &[Repository],
        volumes: &[WorkspaceVolume],
        required: bool,
    ) -> Result<(), OracleError> {
        let (id, name) = (host.peer.id, &host.peer.label);
        self.check_reach(id, name, repositories, required).await?;
        self.check_volumes(id, name, volumes, required).await
    }

    pub async fn create_workspace(
        &self,
        name: &str,
        host: &str,
        repositories: &[Repository],
        volumes: &[WorkspaceVolume],
        by: Option<u64>,
    ) -> Result<Workspace, OracleError> {
        let _workspace_guard = self.workspace_lock.lock().await;
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not a workspace name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        if self.store.workspace_by_name(name)?.is_some() {
            return Err(conflict(format!("a workspace has the name {name} already")));
        }
        let repositories = self.resolve_repositories(repositories)?;
        let volumes = self.resolve_volumes(name, &repositories, volumes)?;
        let host = self.host_by_name(host)?;
        self.check_host(&host, &repositories, &volumes, false)
            .await?;
        let row = self
            .store
            .create_workspace(name, host.peer.id, &repositories, &volumes)?;
        self.audit(by, "workspace_created", &format!("workspace {name}"));
        let workspace = workspace(&row);
        self.emit(Event::WorkspaceChanged {
            workspace: workspace.clone(),
        });
        Ok(workspace)
    }

    pub async fn update_workspace(
        &self,
        name: &str,
        host: Option<&str>,
        repositories: Option<&[Repository]>,
        volumes: Option<&[WorkspaceVolume]>,
        by: Option<u64>,
    ) -> Result<Workspace, OracleError> {
        let _workspace_guard = self.workspace_lock.lock().await;
        let current = self
            .store
            .workspace_by_name(name)?
            .ok_or_else(|| not_found(format!("no workspace has the name {name}")))?;
        let repositories = match repositories {
            Some(repositories) => Some(self.resolve_repositories(repositories)?),
            None => None,
        };
        let checked = repositories.as_ref().unwrap_or(&current.repositories);
        let mounts = volumes.unwrap_or(&current.volumes);
        let mounts = self.resolve_volumes(name, checked, mounts)?;
        let host = match host {
            Some(host) => self.host_by_name(host)?,
            None => self
                .store
                .host(current.host)?
                .ok_or_else(|| not_found("the host of the workspace is gone"))?,
        };
        self.check_host(&host, checked, &mounts, false).await?;
        let row = self
            .store
            .update_workspace(
                name,
                Some(host.peer.id),
                repositories.as_deref(),
                volumes.is_some().then_some(mounts.as_slice()),
            )?
            .ok_or_else(|| not_found(format!("no workspace has the name {name}")))?;
        self.audit(by, "workspace_changed", &format!("workspace {name}"));
        let workspace = workspace(&row);
        self.emit(Event::WorkspaceChanged {
            workspace: workspace.clone(),
        });
        Ok(workspace)
    }

    pub async fn delete_workspace(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        let _workspace_guard = self.workspace_lock.lock().await;
        let closed = self.closed_codespaces(|c| c.workspace_name == name)?;
        if !self.store.delete_workspace(name)? {
            return Err(not_found(format!("no workspace has the name {name}")));
        }
        self.audit(by, "workspace_deleted", &format!("workspace {name}"));
        self.codespaces_removed(closed);
        self.emit(Event::WorkspaceRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    fn closed_codespaces(
        &self,
        filter: impl Fn(&CodespaceRow) -> bool,
    ) -> Result<Vec<String>, OracleError> {
        Ok(self
            .store
            .codespaces()?
            .into_iter()
            .filter(|c| c.state == CodespaceState::Closed && filter(c))
            .map(|c| c.id)
            .collect())
    }

    fn codespaces_removed(&self, ids: Vec<String>) {
        for id in ids {
            self.emit(Event::CodespaceRemoved { id });
        }
    }

    pub fn codespaces(&self) -> Result<Vec<Codespace>, OracleError> {
        Ok(self.store.codespaces()?.iter().map(codespace).collect())
    }

    fn update_codespace(
        &self,
        id: &str,
        change: impl FnOnce(&mut CodespaceRow) -> bool,
    ) -> Result<Option<CodespaceRow>, OracleError> {
        let row = {
            let _guard = self
                .codespace_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(mut row) = self.store.codespace(id)? else {
                return Ok(None);
            };
            if !change(&mut row) {
                return Ok(Some(row));
            }
            row.updated_at = now_unix();
            self.store.save_codespace(&row)?;
            row
        };
        self.emit(Event::CodespaceChanged {
            codespace: codespace(&row),
        });
        Ok(Some(row))
    }

    pub async fn create_codespace(
        self: &Arc<Self>,
        args: NewCodespaceArgs,
        by: Option<u64>,
    ) -> Result<(Codespace, Vec<String>), OracleError> {
        let NewCodespaceArgs {
            workspace,
            preset,
            cpus,
            memory_mib,
            base_branch,
            base_branches,
            client_version,
            first_message,
        } = args;
        let _workspace_guard = self.workspace_lock.lock().await;
        let workspace = self
            .store
            .workspace_by_name(&workspace)?
            .ok_or_else(|| not_found(format!("no workspace has the name {workspace}")))?;
        let preset = preset.map(|name| self.preset_by_name(&name)).transpose()?;
        let first_message = first_message
            .as_ref()
            .map(Oracle::check_first_message)
            .transpose()?;
        let host = self
            .store
            .host(workspace.host)?
            .ok_or_else(|| not_found("the host of the workspace is gone"))?;
        let name = &host.peer.label;
        if !self.host_connected(host.peer.id) {
            return Err(unavailable(format!("the host {name} is not connected")));
        }
        let Some(report) = &host.report else {
            return Err(unavailable(format!("the host {name} has not reported yet")));
        };
        if let Some(branch) = &base_branch
            && !valid_branch(branch)
        {
            return Err(bad_request(format!("{branch:?} is not a branch name")));
        }
        let repositories = super::git::with_base_branches(
            &workspace.repositories,
            base_branch.as_deref(),
            &base_branches,
        )?;
        if client_version.as_ref().is_some_and(|v| {
            v.is_empty() || v.len() > 64 || !v.chars().all(|c| c.is_ascii_graphic())
        }) {
            return Err(bad_request("the client version is not a version"));
        }
        let cpus = cpus.unwrap_or(DEFAULT_CPUS);
        let memory_mib = memory_mib.unwrap_or(DEFAULT_MEMORY_MIB);
        if cpus == 0 || cpus > report.cpus {
            return Err(bad_request(format!(
                "the CPU limit must be 1 to {}, the CPUs of the host {name}",
                report.cpus
            )));
        }
        if memory_mib < MIN_MEMORY_MIB || memory_mib > report.memory_mib {
            return Err(bad_request(format!(
                "the memory limit must be {MIN_MEMORY_MIB} to {} MiB, the memory of the host {name}",
                report.memory_mib
            )));
        }
        self.check_host(&host, &repositories, &workspace.volumes, true)
            .await?;
        let volumes = self.instance_volumes(&workspace.volumes)?;
        let warnings = self.credential_warnings(&repositories, preset.as_ref())?;
        let guard = self
            .codespace_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(max) = report.max_codespaces
            && self.store.count_open_codespaces(host.peer.id)? >= max
        {
            return Err(conflict(format!(
                "the host {name} has its maximum of {max} codespaces"
            )));
        }
        let mut id = new_codespace_id();
        while self.store.codespace(&id)?.is_some() {
            id = new_codespace_id();
        }
        let now = now_unix();
        let row = CodespaceRow {
            id: id.clone(),
            workspace: workspace.id,
            workspace_name: workspace.name.clone(),
            host: host.peer.id,
            host_name: name.clone(),
            preset: preset.as_ref().map(|p| p.id),
            preset_name: preset.as_ref().map(|p| p.name.clone()),
            peer: None,
            state: CodespaceState::Provisioning,
            cpus,
            memory_mib,
            instance: format!("loom-{id}"),
            branch: format!("loom/{id}"),
            base_branch,
            repositories,
            client_version,
            agent: None,
            step: None,
            error: None,
            first_message,
            created_at: now,
            updated_at: now,
        };
        let (_, secret) =
            self.store
                .create_pairing_for(Role::Codespace, BOOTSTRAP_TTL, by, Some(&id))?;
        if let Err(err) = self.store.insert_codespace(&row, &volumes) {
            let _ = self.store.cancel_pairings_for(&id);
            return Err(err.into());
        }
        drop(guard);
        let link = pairing_link(&self.url, &self.identity, &secret);
        self.audit(
            by,
            "codespace_created",
            &format!(
                "codespace {id} of workspace {} on host {name}",
                workspace.name
            ),
        );
        tracing::info!(codespace = %id, workspace = %workspace.name, host = %name, "creating a codespace");
        let created = codespace(&row);
        self.emit(Event::CodespaceChanged {
            codespace: created.clone(),
        });
        self.host_changed(host.peer.id);
        let call = CreateInstance {
            codespace: id,
            instance: row.instance.clone(),
            cpus,
            memory_mib,
            environment: host.environment.clone(),
            bootstrap: Bootstrap {
                link,
                codespace: row.id.clone(),
                workspaces_root: None,
            },
            agent_version: row.client_version.clone(),
            volumes,
        };
        self.volume_hosts_changed(&call.volumes);
        tokio::spawn(self.clone().provision(row, call));
        Ok((created, warnings))
    }

    async fn provision(self: Arc<Self>, row: CodespaceRow, call: CreateInstance) {
        let id = row.id.clone();
        let result = self
            .call_host(
                row.host,
                HostCall::CreateInstance(Box::new(call)),
                CREATE_TIMEOUT,
            )
            .await;
        let update = match result {
            Ok(_) => self.update_codespace(&id, |row| {
                if row.state == CodespaceState::Provisioning {
                    row.state = CodespaceState::Starting;
                    true
                } else {
                    false
                }
            }),
            Err(err) => {
                tracing::warn!(codespace = %id, error = %err, "the host cannot create the instance");
                if let Err(err) = self.store.cancel_pairings_for(&id) {
                    tracing::error!(error = %err, "cannot cancel the pairing of a codespace");
                }
                self.update_codespace(&id, |row| {
                    if matches!(
                        row.state,
                        CodespaceState::Provisioning | CodespaceState::Starting
                    ) {
                        row.state = CodespaceState::Failed;
                        row.error = Some(err.message.clone());
                        true
                    } else {
                        false
                    }
                })
            }
        };
        if let Err(err) = update {
            tracing::error!(codespace = %id, error = %err, "cannot save the state of a codespace");
        }
    }

    pub fn close_codespace(
        self: &Arc<Self>,
        id: &str,
        by: Option<u64>,
    ) -> Result<Codespace, OracleError> {
        let mut start = false;
        let row = self
            .update_codespace(id, |row| match row.state {
                CodespaceState::Closed | CodespaceState::Closing => false,
                _ => {
                    row.state = CodespaceState::Closing;
                    row.error = None;
                    start = true;
                    true
                }
            })?
            .ok_or_else(|| not_found(format!("no codespace has the ID {id}")))?;
        if start {
            self.audit(by, "codespace_closed", &format!("codespace {id}"));
            tracing::info!(codespace = %id, "closing a codespace");
            self.revoke_codespace(&row);
            tokio::spawn(self.clone().delete_instance(row.clone()));
        }
        Ok(codespace(&row))
    }

    fn revoke_codespace(&self, row: &CodespaceRow) {
        if let Some(peer) = row.peer {
            if let Err(err) = self.store.remove_peer(peer) {
                tracing::error!(error = %err, "cannot remove the peer of a codespace");
            }
            self.close_sessions(peer);
        }
        if let Err(err) = self.store.cancel_pairings_for(&row.id) {
            tracing::error!(error = %err, "cannot cancel the pairing of a codespace");
        }
        if let Some(host) = row
            .agent
            .as_ref()
            .and_then(|agent| agent.host_id.parse::<HostId>().ok())
        {
            self.hosts.remove_codespace(&host);
            self.relay.disconnect_host(&host);
        }
    }

    async fn delete_instance(self: Arc<Self>, row: CodespaceRow) {
        let call = HostCall::DeleteInstance {
            codespace: row.id.clone(),
            instance: row.instance.clone(),
        };
        let result = self.call_host(row.host, call, DELETE_TIMEOUT).await;
        let id = &row.id;
        let update = match result {
            Ok(_) => self.update_codespace(id, |row| {
                if row.state == CodespaceState::Closing {
                    row.state = CodespaceState::Closed;
                    row.error = None;
                    row.step = None;
                    true
                } else {
                    false
                }
            }),
            Err(err) => {
                tracing::warn!(codespace = %id, error = %err, "cannot delete the instance yet");
                self.update_codespace(id, |row| {
                    row.error = Some(format!(
                        "the instance is not deleted yet: {}; the oracle tries again when the host reports",
                        err.message
                    ));
                    row.state == CodespaceState::Closing
                })
            }
        };
        if let Err(err) = update {
            tracing::error!(codespace = %id, error = %err, "cannot save the state of a codespace");
        }
        self.host_changed(row.host);
    }

    pub fn host_report(&self, host: u64, report: &HostReport) -> Result<(), OracleError> {
        let mut report = report.clone();
        if !report
            .connection_types
            .iter()
            .any(|kind| kind == CONNECTION_RELAY)
        {
            report.connection_types.insert(0, CONNECTION_RELAY.into());
        }
        self.store.set_host_report(host, &report)?;
        self.host_changed(host);
        Ok(())
    }

    pub fn host_instances(
        self: &Arc<Self>,
        host: u64,
        instances: Vec<InstanceReport>,
    ) -> Result<(), OracleError> {
        let me = self.identity.host_id().to_string();
        let reported: HashMap<String, InstanceReport> = instances
            .into_iter()
            .filter(|instance| instance.oracle == me)
            .map(|instance| (instance.codespace.clone(), instance))
            .collect();
        let rows: HashMap<String, CodespaceRow> = self
            .store
            .host_codespaces(host)?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        for (id, instance) in &reported {
            let orphan = rows.get(id).is_none_or(|row| {
                matches!(row.state, CodespaceState::Closing | CodespaceState::Closed)
            });
            if orphan {
                tracing::info!(codespace = %id, instance = %instance.instance, "deleting an instance that has no open codespace");
                let row = rows.get(id).cloned().unwrap_or_else(|| CodespaceRow {
                    id: id.clone(),
                    workspace: 0,
                    workspace_name: String::new(),
                    host,
                    host_name: String::new(),
                    preset: None,
                    preset_name: None,
                    peer: None,
                    state: CodespaceState::Closed,
                    cpus: 0,
                    memory_mib: 0,
                    instance: instance.instance.clone(),
                    branch: String::new(),
                    base_branch: None,
                    repositories: Vec::new(),
                    client_version: None,
                    agent: None,
                    step: None,
                    error: None,
                    first_message: None,
                    created_at: 0,
                    updated_at: 0,
                });
                tokio::spawn(self.clone().delete_instance(row));
            }
        }
        let now = now_unix();
        for row in rows.values() {
            if reported.contains_key(&row.id) {
                continue;
            }
            let settled = now.saturating_sub(row.updated_at) >= LOST_GRACE_SECS;
            let result = match row.state {
                CodespaceState::Starting | CodespaceState::Ready if settled => {
                    tracing::warn!(codespace = %row.id, "the host does not have the instance of a codespace");
                    self.update_codespace(&row.id, |row| {
                        let open =
                            matches!(row.state, CodespaceState::Starting | CodespaceState::Ready);
                        if open {
                            row.state = CodespaceState::Lost;
                            row.error = Some("the host does not have the instance".into());
                        }
                        open
                    })
                }
                CodespaceState::Closing => self.update_codespace(&row.id, |row| {
                    let closing = row.state == CodespaceState::Closing;
                    if closing {
                        row.state = CodespaceState::Closed;
                        row.error = None;
                        row.step = None;
                    }
                    closing
                }),
                _ => Ok(None),
            };
            if let Err(err) = result {
                tracing::error!(codespace = %row.id, error = %err, "cannot save the state of a codespace");
            }
        }
        Ok(())
    }

    pub fn instance_progress(&self, host: u64, id: &str, step: &str) -> Result<(), OracleError> {
        let row = self
            .store
            .codespace(id)?
            .filter(|row| row.host == host)
            .ok_or_else(|| not_found(format!("this host has no codespace {id}")))?;
        self.progress(&row, step)
    }

    fn progress(&self, row: &CodespaceRow, step: &str) -> Result<(), OracleError> {
        let step: String = step.chars().take(200).collect();
        self.update_codespace(&row.id, |row| {
            row.step = Some(step.clone());
            row.state.is_open()
        })?;
        self.emit(Event::CodespaceProgress {
            id: row.id.clone(),
            step,
        });
        Ok(())
    }

    pub fn codespace_of_peer(&self, peer: u64) -> Result<CodespaceRow, OracleError> {
        self.store
            .codespace_by_peer(peer)?
            .ok_or_else(|| forbidden("this peer has no codespace"))
    }

    pub fn bundle(&self, peer: u64) -> Result<Bundle, OracleError> {
        let row = self.codespace_of_peer(peer)?;
        let workspace = self
            .store
            .workspaces()?
            .into_iter()
            .find(|workspace| workspace.id == row.workspace)
            .ok_or_else(|| not_found("the workspace of the codespace is gone"))?;
        let repositories = if row.repositories.is_empty() {
            super::git::with_base_branches(
                &workspace.repositories,
                row.base_branch.as_deref(),
                &BTreeMap::new(),
            )?
        } else {
            row.repositories.clone()
        };
        let git = self.git_setup(row.preset, &repositories)?;
        let agent = self.preset_bundle(row.preset)?;
        let notes = self.store.notes(workspace.id)?.map(|(text, _)| text);
        let workspace_name = workspace.name.clone();
        Ok(Bundle {
            codespace: row.id.clone(),
            workspace: workspace_name,
            branch: row.branch.clone(),
            repositories,
            devices: self.device_keys()?,
            relay_url: self.url.clone(),
            preset: row.preset_name.clone(),
            git,
            agent,
            notes,
        })
    }

    pub fn device_keys(&self) -> Result<Vec<DeviceKey>, OracleError> {
        Ok(self
            .store
            .peers(Some(Role::Device))?
            .into_iter()
            .map(|peer| DeviceKey {
                label: peer.label,
                pubkey: loom_crypto::encode_key(&peer.pubkey),
                push_token: peer.push_token,
            })
            .collect())
    }

    pub fn codespace_agent(&self, peer: u64, address: &AgentAddress) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        let host: HostId = address
            .host_id
            .parse()
            .map_err(|_| bad_request("the host ID of the agent is not 64 hex characters"))?;
        if decode_key(&address.host_key).is_none() {
            return Err(bad_request("the host key of the agent is not a key"));
        }
        if address
            .instance
            .as_ref()
            .is_some_and(|i| i.is_empty() || i.len() > 128)
        {
            return Err(bad_request("the instance ID of the agent is not valid"));
        }
        if !matches!(
            row.state,
            CodespaceState::Provisioning | CodespaceState::Starting | CodespaceState::Ready
        ) {
            return Err(conflict(format!("the codespace is {}", row.state)));
        }
        if let Some(old) = row
            .agent
            .as_ref()
            .and_then(|agent| agent.host_id.parse::<HostId>().ok())
            && old != host
        {
            self.hosts.remove_codespace(&old);
            self.relay.disconnect_host(&old);
        }
        self.hosts.insert_codespace(host);
        let address = address.clone();
        self.update_codespace(&row.id, |row| {
            row.agent = Some(address);
            if row.state == CodespaceState::Provisioning {
                row.state = CodespaceState::Starting;
            }
            true
        })?;
        Ok(())
    }

    pub fn codespace_progress(&self, peer: u64, step: &str) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        self.progress(&row, step)
    }

    pub fn codespace_ready(&self, peer: u64) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        let mut refused = None;
        self.update_codespace(&row.id, |row| match row.state {
            CodespaceState::Provisioning
            | CodespaceState::Starting
            | CodespaceState::Lost
            | CodespaceState::Failed => {
                row.state = CodespaceState::Ready;
                row.error = None;
                row.step = None;
                true
            }
            CodespaceState::Ready => {
                let changed = row.step.is_some() || row.error.is_some();
                row.step = None;
                row.error = None;
                changed
            }
            other => {
                refused = Some(other);
                false
            }
        })?;
        if let Some(state) = refused {
            return Err(conflict(format!("the codespace is {state}")));
        }
        tracing::info!(codespace = %row.id, "a codespace is ready");
        Ok(())
    }

    pub fn codespace_failed(&self, peer: u64, error: &str) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        let error: String = error.chars().take(2000).collect();
        tracing::warn!(codespace = %row.id, %error, "a codespace failed");
        self.update_codespace(&row.id, |row| {
            let open = matches!(
                row.state,
                CodespaceState::Provisioning | CodespaceState::Starting | CodespaceState::Ready
            );
            if open {
                row.state = CodespaceState::Failed;
                row.error = Some(error);
            }
            open
        })?;
        Ok(())
    }

    pub(super) fn check_codespace_pairing(&self, target: Option<&str>) -> Result<String, String> {
        let Some(id) = target else {
            return Err("the pairing code has no codespace".into());
        };
        match self.store.codespace(id) {
            Ok(Some(row))
                if matches!(
                    row.state,
                    CodespaceState::Provisioning | CodespaceState::Starting
                ) =>
            {
                Ok(row.id)
            }
            Ok(_) => Err(format!("the codespace {id} does not wait for its agent")),
            Err(err) => {
                tracing::error!(error = %err, "cannot read a codespace");
                Err("the oracle cannot read the codespace".into())
            }
        }
    }

    pub(super) fn codespace_paired(&self, id: &str, peer: u64) {
        let result = self.update_codespace(id, |row| {
            row.peer = Some(peer);
            true
        });
        if let Err(err) = result {
            tracing::error!(codespace = %id, error = %err, "cannot link a codespace to its agent");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codespace_ids_start_with_a_letter() {
        for _ in 0..100 {
            let id = new_codespace_id();
            assert_eq!(id.len(), 8);
            assert!(valid_name(&id), "{id}");
        }
    }

    #[test]
    fn branches_follow_the_rules_of_git() {
        for name in ["main", "release/1.2", "feature-x_y"] {
            assert!(valid_branch(name), "{name}");
        }
        for name in [
            "", "-x", "a..b", "a b", "a:b", "x.lock", "a/", "/a", "a//b", "a@{1",
        ] {
            assert!(!valid_branch(name), "{name}");
        }
    }

    #[test]
    fn environments_are_checked() {
        let mut environment = Environment::default();
        assert!(validate_environment(&environment).is_ok());
        environment
            .variables
            .insert("HTTP_PROXY".into(), "http://proxy:3128".into());
        environment.base_image = Some("images:debian/13".into());
        assert!(validate_environment(&environment).is_ok());
        environment.variables.insert("1X".into(), "y".into());
        assert!(validate_environment(&environment).is_err());
        environment.variables.remove("1X");
        environment.ca_certificates.push("not a certificate".into());
        assert!(validate_environment(&environment).is_err());
        environment.ca_certificates.clear();
        environment
            .docker
            .registry_mirrors
            .push("mirror.local".into());
        assert!(validate_environment(&environment).is_err());
    }
}
