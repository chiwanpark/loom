use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use loom_config::HumanDuration;
use loom_oracle_proto::{
    AuditEntry, AuditLog, CONNECTION_RELAY, CONNECTION_SSH, Codespace, CodespaceConnection,
    CodespaceMetrics, CodespaceState, DEFAULT_AUDIT_LIMIT, ErrorCode, Event, HostCall, HostLimits,
    MAX_AUDIT_LIMIT, OracleError, Role, ssh_command,
};
use loom_proto::relay::HostId;
use tokio_util::sync::CancellationToken;

use super::codespaces::{codespace, conflict};
use super::{Oracle, bad_request, not_found};
use crate::store::{CodespaceRow, HostRow, now_unix};

const STOP_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const START_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MIN_IDLE_STOP_SECS: u64 = 60;
const MIN_RETENTION_SECS: u64 = 3600;
pub const STEP_STOPPING: &str = "stopping";
pub const STEP_RESUMING: &str = "resuming";

fn unavailable(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Unavailable, message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Timeouts {
    idle_stop: u64,
    retention: u64,
}

fn agent_host(row: &CodespaceRow) -> Option<HostId> {
    row.agent.as_ref()?.host_id.parse().ok()
}

fn stop_at(row: &CodespaceRow, timeouts: Timeouts) -> Option<u64> {
    let idle = row.state == CodespaceState::Ready
        && row.active == Some(false)
        && row.first_message.is_none()
        && timeouts.idle_stop > 0;
    idle.then(|| {
        row.active_at
            .unwrap_or(row.updated_at)
            .saturating_add(timeouts.idle_stop)
    })
}

fn delete_at(row: &CodespaceRow, timeouts: Timeouts) -> Option<u64> {
    (row.state == CodespaceState::Stopped && timeouts.retention > 0).then(|| {
        row.stopped_at
            .unwrap_or(row.updated_at)
            .saturating_add(timeouts.retention)
    })
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub fn validate_limits(limits: &HostLimits) -> Result<(), OracleError> {
    if limits.max_codespaces == Some(0) {
        return Err(bad_request("the maximum of codespaces must be 1 or more"));
    }
    if limits.max_running == Some(0) {
        return Err(bad_request(
            "the maximum of running codespaces must be 1 or more",
        ));
    }
    if let Some(secs) = limits.idle_stop_secs
        && secs > 0
        && secs < MIN_IDLE_STOP_SECS
    {
        return Err(bad_request(format!(
            "the idle timeout must be 0 (never) or {MIN_IDLE_STOP_SECS} seconds or more"
        )));
    }
    if let Some(secs) = limits.retention_secs
        && secs > 0
        && secs < MIN_RETENTION_SECS
    {
        return Err(bad_request(format!(
            "the retention must be 0 (never) or {MIN_RETENTION_SECS} seconds or more"
        )));
    }
    Ok(())
}

pub(super) async fn run(oracle: Arc<Oracle>, shutdown: CancellationToken) {
    let interval = oracle.lifecycle.check_interval.as_duration();
    oracle.clean_archives();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        oracle.check_lifecycle(now_unix());
    }
}

impl Oracle {
    fn lock_transitions(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<String>> {
        self.transitions.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn in_transition(&self, id: &str) -> bool {
        self.lock_transitions().contains(id)
    }

    fn begin_transition(&self, id: &str) -> bool {
        self.lock_transitions().insert(id.to_owned())
    }

    fn end_transition(&self, id: &str) {
        self.lock_transitions().remove(id);
    }

    pub(super) fn lifecycle_limits(&self) -> Result<HashMap<u64, HostLimits>, OracleError> {
        Ok(self
            .store
            .hosts()?
            .into_iter()
            .map(|host| (host.peer.id, host.limits))
            .collect())
    }

    fn timeouts(&self, limits: Option<&HostLimits>) -> Timeouts {
        let limits = limits.copied().unwrap_or_default();
        Timeouts {
            idle_stop: limits
                .idle_stop_secs
                .unwrap_or_else(|| self.lifecycle.idle_stop.as_duration().as_secs()),
            retention: limits
                .retention_secs
                .unwrap_or_else(|| self.lifecycle.retention.as_duration().as_secs()),
        }
    }

    pub(super) fn view_with(
        &self,
        row: &CodespaceRow,
        limits: &HashMap<u64, HostLimits>,
    ) -> Codespace {
        let timeouts = self.timeouts(limits.get(&row.host));
        let mut view = codespace(row);
        view.stop_at = stop_at(row, timeouts);
        view.delete_at = delete_at(row, timeouts);
        view.archive = super::archive::archive_view(
            row,
            self.lifecycle.archive_retention.as_duration().as_secs(),
        );
        view
    }

    pub(super) fn view(&self, row: &CodespaceRow) -> Codespace {
        let limits = match self.store.host(row.host) {
            Ok(Some(host)) => HashMap::from([(host.peer.id, host.limits)]),
            _ => HashMap::new(),
        };
        self.view_with(row, &limits)
    }

    pub(super) fn check_host_room(&self, host: &HostRow, new: bool) -> Result<(), OracleError> {
        let name = &host.peer.label;
        if new {
            let open = self.store.count_open_codespaces(host.peer.id)?;
            let local = host.report.as_ref().and_then(|r| r.max_codespaces);
            if let Some(max) = [local, host.limits.max_codespaces]
                .into_iter()
                .flatten()
                .min()
                && open >= max
            {
                return Err(conflict(format!(
                    "the host {name} has its maximum of {max} codespaces; close one first"
                )));
            }
        }
        if let Some(max) = host.limits.max_running
            && self.store.count_running_codespaces(host.peer.id)? >= max
        {
            return Err(conflict(format!(
                "the host {name} has its maximum of {max} running codespaces; stop or close one first"
            )));
        }
        Ok(())
    }

    fn admit(&self, row: &CodespaceRow) {
        if let Some(host) = agent_host(row) {
            self.hosts.insert_codespace(host);
        }
    }

    fn dismiss(&self, row: &CodespaceRow) {
        if let Some(host) = agent_host(row) {
            self.hosts.remove_codespace(&host);
            self.relay.disconnect_host(&host);
        }
        if let Some(peer) = row.peer {
            self.close_sessions(peer);
        }
    }

    pub(super) fn instance_running(&self, row: &CodespaceRow, running: bool) {
        let result = match (row.state, running) {
            (CodespaceState::Ready, false) => {
                tracing::warn!(codespace = %row.id, "the instance of a codespace stopped");
                let changed = self.update_codespace(&row.id, |row| {
                    let ready = row.state == CodespaceState::Ready;
                    if ready {
                        row.state = CodespaceState::Stopped;
                        row.stopped_at = Some(now_unix());
                        row.active = None;
                        row.step = None;
                        row.error = Some("the instance stopped outside Loom".into());
                    }
                    ready
                });
                if let Ok(Some(row)) = &changed
                    && row.state == CodespaceState::Stopped
                {
                    self.dismiss(row);
                }
                changed
            }
            (CodespaceState::Stopped, true) => {
                tracing::info!(codespace = %row.id, "the instance of a stopped codespace runs again");
                self.admit(row);
                self.update_codespace(&row.id, |row| {
                    let stopped = row.state == CodespaceState::Stopped;
                    if stopped {
                        row.state = CodespaceState::Starting;
                        row.step = Some(STEP_RESUMING.into());
                        row.error = None;
                    }
                    stopped
                })
            }
            _ => return,
        };
        if let Err(err) = result {
            tracing::error!(codespace = %row.id, error = %err, "cannot save the state of a codespace");
        }
        self.host_changed(row.host);
    }

    pub fn stop_codespace(
        self: &Arc<Self>,
        id: &str,
        by: Option<u64>,
    ) -> Result<Codespace, OracleError> {
        self.stop_with(id, by, "codespace_stopped")
    }

    fn stop_with(
        self: &Arc<Self>,
        id: &str,
        by: Option<u64>,
        action: &str,
    ) -> Result<Codespace, OracleError> {
        let current = self
            .store
            .codespace(id)?
            .ok_or_else(|| not_found(format!("no codespace has the ID {id}")))?;
        if !self.host_connected(current.host) {
            return Err(unavailable(format!(
                "the host {} is not connected",
                current.host_name
            )));
        }
        if !self.begin_transition(id) {
            return Err(conflict(format!(
                "the codespace {id} is stopping or starting already"
            )));
        }
        let mut refused = None;
        let changed = self.update_codespace(id, |row| match row.state {
            CodespaceState::Ready | CodespaceState::Starting | CodespaceState::Failed => {
                row.step = Some(STEP_STOPPING.into());
                row.error = None;
                true
            }
            other => {
                refused = Some(other);
                false
            }
        });
        let row = match (changed, refused) {
            (Ok(Some(row)), None) => row,
            (Ok(_), Some(state)) => {
                self.end_transition(id);
                return Err(conflict(format!(
                    "the codespace {id} is {state}; only a running codespace can stop"
                )));
            }
            (Ok(None), None) => {
                self.end_transition(id);
                return Err(not_found(format!("no codespace has the ID {id}")));
            }
            (Err(err), _) => {
                self.end_transition(id);
                return Err(err);
            }
        };
        self.audit(by, action, &format!("codespace {id}"));
        tracing::info!(codespace = %id, action, "stopping a codespace");
        tokio::spawn(self.clone().finish_stop(row.clone()));
        Ok(self.view(&row))
    }

    async fn finish_stop(self: Arc<Self>, row: CodespaceRow) {
        let call = HostCall::StopInstance {
            codespace: row.id.clone(),
            instance: row.instance.clone(),
        };
        let result = self.call_host(row.host, call, STOP_TIMEOUT).await;
        let update = match result {
            Ok(_) => {
                let changed = self.update_codespace(&row.id, |row| {
                    let running = matches!(
                        row.state,
                        CodespaceState::Ready | CodespaceState::Starting | CodespaceState::Failed
                    );
                    if running {
                        row.state = CodespaceState::Stopped;
                        row.stopped_at = Some(now_unix());
                        row.active = None;
                        row.error = None;
                    }
                    if row.step.as_deref() == Some(STEP_STOPPING) {
                        row.step = None;
                    }
                    true
                });
                if let Ok(Some(row)) = &changed
                    && row.state == CodespaceState::Stopped
                {
                    self.dismiss(row);
                    tracing::info!(codespace = %row.id, "stopped a codespace");
                }
                changed
            }
            Err(err) => {
                tracing::warn!(codespace = %row.id, error = %err, "cannot stop a codespace");
                self.update_codespace(&row.id, |row| {
                    if row.step.as_deref() == Some(STEP_STOPPING) {
                        row.step = None;
                    }
                    row.error = Some(format!("cannot stop the codespace: {}", err.message));
                    true
                })
            }
        };
        if let Err(err) = update {
            tracing::error!(codespace = %row.id, error = %err, "cannot save the state of a codespace");
        }
        self.end_transition(&row.id);
        self.host_changed(row.host);
    }

    pub fn resume_codespace(
        self: &Arc<Self>,
        id: &str,
        by: Option<u64>,
    ) -> Result<Codespace, OracleError> {
        let current = self
            .store
            .codespace(id)?
            .ok_or_else(|| not_found(format!("no codespace has the ID {id}")))?;
        let host = self
            .store
            .host(current.host)?
            .ok_or_else(|| not_found("the host of the codespace is gone"))?;
        if !self.host_connected(host.peer.id) {
            return Err(unavailable(format!(
                "the host {} is not connected",
                host.peer.label
            )));
        }
        if !self.begin_transition(id) {
            return Err(conflict(format!(
                "the codespace {id} is stopping or starting already"
            )));
        }
        let mut refused = None;
        let changed = self.update_codespace(id, |row| {
            if row.state != CodespaceState::Stopped {
                refused = Some(conflict(format!(
                    "the codespace {id} is {}; only a stopped codespace can resume",
                    row.state
                )));
                return false;
            }
            if let Err(err) = self.check_host_room(&host, false) {
                refused = Some(err);
                return false;
            }
            row.state = CodespaceState::Starting;
            row.step = Some(STEP_RESUMING.into());
            row.error = None;
            row.active = None;
            true
        });
        let row = match (changed, refused) {
            (Ok(Some(row)), None) => row,
            (Ok(_), Some(err)) | (Err(err), _) => {
                self.end_transition(id);
                return Err(err);
            }
            (Ok(None), None) => {
                self.end_transition(id);
                return Err(not_found(format!("no codespace has the ID {id}")));
            }
        };
        self.admit(&row);
        self.audit(by, "codespace_resumed", &format!("codespace {id}"));
        tracing::info!(codespace = %id, "resuming a codespace");
        self.host_changed(row.host);
        tokio::spawn(self.clone().finish_resume(row.clone()));
        Ok(self.view(&row))
    }

    async fn finish_resume(self: Arc<Self>, row: CodespaceRow) {
        let call = HostCall::StartInstance {
            codespace: row.id.clone(),
            instance: row.instance.clone(),
        };
        if let Err(err) = self.call_host(row.host, call, START_TIMEOUT).await {
            tracing::warn!(codespace = %row.id, error = %err, "cannot resume a codespace");
            let update = self.update_codespace(&row.id, |row| {
                let starting = row.state == CodespaceState::Starting;
                if starting {
                    row.state = CodespaceState::Stopped;
                    row.step = None;
                    row.error = Some(format!("cannot start the instance: {}", err.message));
                }
                starting
            });
            match update {
                Ok(Some(row)) if row.state == CodespaceState::Stopped => self.dismiss(&row),
                Ok(_) => {}
                Err(err) => {
                    tracing::error!(codespace = %row.id, error = %err, "cannot save the state of a codespace")
                }
            }
        }
        self.end_transition(&row.id);
        self.host_changed(row.host);
    }

    pub fn set_host_limits(
        &self,
        name: &str,
        limits: &HostLimits,
        by: Option<u64>,
    ) -> Result<loom_oracle_proto::Host, OracleError> {
        validate_limits(limits)?;
        let row = self
            .store
            .host_by_name(name)?
            .ok_or_else(|| not_found(format!("no host has the name {name}")))?;
        self.store.set_host_limits(row.peer.id, limits)?;
        self.audit(by, "host_limits_set", &format!("host {name}"));
        let mut changed = HashMap::new();
        changed.insert(row.peer.id, *limits);
        for codespace in self.store.host_codespaces(row.peer.id)? {
            if matches!(
                codespace.state,
                CodespaceState::Ready | CodespaceState::Stopped
            ) {
                self.emit(Event::CodespaceChanged {
                    codespace: self.view_with(&codespace, &changed),
                });
            }
        }
        self.host_changed(row.peer.id);
        self.hosts()?
            .into_iter()
            .find(|host| host.name == name)
            .ok_or_else(|| not_found(format!("no host has the name {name}")))
    }

    pub fn codespace_activity(
        &self,
        peer: u64,
        active: bool,
        metrics: Option<CodespaceMetrics>,
    ) -> Result<(), OracleError> {
        let row = self.codespace_of_peer(peer)?;
        self.change_codespace(&row.id, false, |row| {
            if active || row.active != Some(false) {
                row.active_at = Some(now_unix());
            }
            row.active = Some(active);
            if metrics.is_some() {
                row.metrics = metrics;
            }
            true
        })?;
        Ok(())
    }

    pub fn codespace_connections(
        &self,
        id: &str,
        types: &[String],
    ) -> Result<Vec<CodespaceConnection>, OracleError> {
        let row = self
            .store
            .codespace(id)?
            .ok_or_else(|| not_found(format!("no codespace has the ID {id}")))?;
        if !matches!(row.state, CodespaceState::Ready | CodespaceState::Starting) {
            return Err(conflict(match row.state {
                CodespaceState::Stopped => {
                    format!("the codespace {id} is stopped; resume it first")
                }
                state => format!("the codespace {id} is {state}"),
            }));
        }
        let wants = |kind: &str| types.is_empty() || types.iter().any(|t| t == kind);
        let host = self.store.host(row.host)?;
        let mut connections = Vec::new();
        if wants(CONNECTION_SSH)
            && let Some(destination) = host.and_then(|host| host.ssh)
        {
            connections.push(CodespaceConnection::Ssh {
                destination,
                command: ssh_command(&row.id),
            });
        }
        if wants(CONNECTION_RELAY)
            && let Some(address) = row.agent
        {
            connections.push(CodespaceConnection::Relay { address });
        }
        if connections.is_empty() {
            return Err(unavailable(format!(
                "the codespace {id} has no connection type that this client supports yet"
            )));
        }
        Ok(connections)
    }

    pub fn audit_entries(
        &self,
        limit: Option<u32>,
        before: Option<u64>,
    ) -> Result<AuditLog, OracleError> {
        let limit = limit
            .unwrap_or(DEFAULT_AUDIT_LIMIT)
            .clamp(1, MAX_AUDIT_LIMIT);
        let entries = self
            .store
            .audit_page(limit as usize, before)?
            .into_iter()
            .map(|row| AuditEntry {
                id: row.id,
                at: row.at,
                peer: row.peer,
                peer_label: row.peer_label,
                action: row.action,
                detail: row.detail,
            })
            .collect();
        Ok(AuditLog { entries })
    }

    pub fn check_lifecycle(self: &Arc<Self>, now: u64) {
        let rows = match self.store.codespaces() {
            Ok(rows) => rows,
            Err(err) => {
                tracing::error!(error = %err, "cannot read the codespaces");
                return;
            }
        };
        let limits = match self.lifecycle_limits() {
            Ok(limits) => limits,
            Err(err) => {
                tracing::error!(error = %err, "cannot read the hosts");
                return;
            }
        };
        for row in rows {
            if self.in_transition(&row.id) {
                continue;
            }
            let timeouts = self.timeouts(limits.get(&row.host));
            let result = if stop_at(&row, timeouts).is_some_and(|at| now >= at)
                && self.host_connected(row.host)
            {
                tracing::info!(codespace = %row.id, "stopping an idle codespace");
                self.stop_with(&row.id, None, "codespace_idle_stopped")
                    .map(|_| ())
            } else if delete_at(&row, timeouts).is_some_and(|at| now >= at) {
                tracing::info!(codespace = %row.id, "deleting a codespace after its retention");
                self.close_with(&row.id, None, "codespace_expired")
                    .map(|_| ())
            } else {
                Ok(())
            };
            if let Err(err) = result {
                tracing::warn!(codespace = %row.id, error = %err, "cannot apply the lifecycle of a codespace");
            }
        }
        let before = |keep: HumanDuration| {
            let keep = keep.as_duration().as_secs();
            (keep > 0).then(|| now.saturating_sub(keep))
        };
        let closed = before(self.lifecycle.closed_retention);
        let archived = before(self.lifecycle.archive_retention);
        if closed.is_some() || archived.is_some() {
            match self.store.delete_closed_codespaces(closed, archived) {
                Ok(ids) => {
                    for id in ids {
                        self.remove_archive(&id);
                        self.emit(Event::CodespaceRemoved { id });
                    }
                }
                Err(err) => tracing::error!(error = %err, "cannot remove closed codespaces"),
            }
        }
    }

    pub fn metrics_text(&self) -> String {
        let mut text = self.relay.metrics_text();
        let rows = self.store.codespaces().unwrap_or_default();
        let hosts = self.store.hosts().unwrap_or_default();
        let devices = self.store.peers(Some(Role::Device)).unwrap_or_default();
        let mut gauge = |name: &str, help: &str, values: Vec<(String, u64)>| {
            let _ = writeln!(text, "# HELP {name} {help}");
            let _ = writeln!(text, "# TYPE {name} gauge");
            for (labels, value) in values {
                let _ = writeln!(text, "{name}{labels} {value}");
            }
        };
        gauge(
            "loom_oracle_codespaces",
            "Codespaces by state",
            CodespaceState::ALL
                .into_iter()
                .map(|state| {
                    let count = rows.iter().filter(|row| row.state == state).count();
                    (format!("{{state=\"{state}\"}}"), count as u64)
                })
                .collect(),
        );
        let active = rows
            .iter()
            .filter(|row| row.state == CodespaceState::Ready && row.active == Some(true))
            .count();
        gauge(
            "loom_oracle_codespaces_active",
            "Ready codespaces with a client, a running turn, or a terminal",
            vec![(String::new(), active as u64)],
        );
        let sum = |field: fn(&CodespaceMetrics) -> u64| {
            rows.iter()
                .filter(|row| row.state.is_running())
                .filter_map(|row| row.metrics.as_ref())
                .map(field)
                .sum::<u64>()
        };
        gauge(
            "loom_oracle_codespace_sessions",
            "Sessions in running codespaces",
            vec![(String::new(), sum(|m| m.sessions))],
        );
        gauge(
            "loom_oracle_codespace_busy_sessions",
            "Sessions with a running turn in running codespaces",
            vec![(String::new(), sum(|m| m.busy_sessions))],
        );
        gauge(
            "loom_oracle_codespace_connections",
            "Client connections to running codespaces",
            vec![(String::new(), sum(|m| m.connections))],
        );
        gauge(
            "loom_oracle_hosts_connected",
            "Connected hosts",
            vec![(
                String::new(),
                hosts
                    .iter()
                    .filter(|host| self.host_connected(host.peer.id))
                    .count() as u64,
            )],
        );
        gauge(
            "loom_oracle_devices_connected",
            "Connected devices",
            vec![(
                String::new(),
                devices
                    .iter()
                    .filter(|device| self.is_connected(device.id))
                    .count() as u64,
            )],
        );
        let per_host = |value: &dyn Fn(&CodespaceRow) -> u64| {
            hosts
                .iter()
                .map(|host| {
                    let total = rows
                        .iter()
                        .filter(|row| row.host == host.peer.id && row.state.is_running())
                        .map(value)
                        .sum();
                    (
                        format!("{{host=\"{}\"}}", escape_label(&host.peer.label)),
                        total,
                    )
                })
                .collect::<Vec<_>>()
        };
        gauge(
            "loom_oracle_host_codespaces_running",
            "Running codespaces of each host",
            per_host(&|_| 1),
        );
        gauge(
            "loom_oracle_host_cpus_limit",
            "Sum of the CPU limits of the running codespaces of each host",
            per_host(&|row| u64::from(row.cpus)),
        );
        gauge(
            "loom_oracle_host_memory_limit_bytes",
            "Sum of the memory limits of the running codespaces of each host",
            per_host(&|row| row.memory_mib * 1024 * 1024),
        );
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_checked() {
        assert!(validate_limits(&HostLimits::default()).is_ok());
        for bad in [
            HostLimits {
                max_codespaces: Some(0),
                ..Default::default()
            },
            HostLimits {
                max_running: Some(0),
                ..Default::default()
            },
            HostLimits {
                idle_stop_secs: Some(5),
                ..Default::default()
            },
            HostLimits {
                retention_secs: Some(60),
                ..Default::default()
            },
        ] {
            assert!(validate_limits(&bad).is_err(), "{bad:?}");
        }
        let never = HostLimits {
            idle_stop_secs: Some(0),
            retention_secs: Some(0),
            ..Default::default()
        };
        assert!(validate_limits(&never).is_ok());
    }

    #[test]
    fn labels_are_escaped() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
