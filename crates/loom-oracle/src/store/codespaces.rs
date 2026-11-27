use loom_oracle_proto::{
    AgentAddress, CodespaceMetrics, CodespaceState, Environment, HostCheck, HostLimits, HostReport,
    InstanceVolume, Repository, Role, WorkspaceVolume,
};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::volumes::{put_codespace_volumes, put_workspace_volumes, workspace_volumes};
use super::{
    PEER_COLUMNS, PeerRow, Store, StoreError, StoreResult, now_unix, peer_row, to_i64, to_u64,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    pub peer: PeerRow,
    pub report: Option<HostReport>,
    pub reported_at: Option<u64>,
    pub environment: Environment,
    pub ssh: Option<String>,
    pub checks: Vec<HostCheck>,
    pub checked_at: Option<u64>,
    pub limits: HostLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRow {
    pub id: u64,
    pub name: String,
    pub host: u64,
    pub host_name: String,
    pub repositories: Vec<Repository>,
    pub volumes: Vec<WorkspaceVolume>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodespaceRow {
    pub id: String,
    pub workspace: u64,
    pub workspace_name: String,
    pub host: u64,
    pub host_name: String,
    pub preset: Option<u64>,
    pub preset_name: Option<String>,
    pub peer: Option<u64>,
    pub state: CodespaceState,
    pub cpus: u32,
    pub memory_mib: u64,
    pub instance: String,
    pub branch: String,
    pub base_branch: Option<String>,
    pub repositories: Vec<Repository>,
    pub client_version: Option<String>,
    pub agent: Option<AgentAddress>,
    pub step: Option<String>,
    pub error: Option<String>,
    pub first_message: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub stopped_at: Option<u64>,
    pub active: Option<bool>,
    pub active_at: Option<u64>,
    pub metrics: Option<CodespaceMetrics>,
    pub archive: Option<ArchiveRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveRow {
    pub size: u64,
    pub saved_at: Option<u64>,
    pub error: Option<String>,
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".into())
}

fn from_json<T: DeserializeOwned>(text: &str, what: &'static str) -> StoreResult<T> {
    serde_json::from_str(text).map_err(|_| StoreError::Malformed(what))
}

pub(super) fn add_host_peer(
    tx: Transaction<'_>,
    label: &str,
    pubkey: &[u8; 32],
    pairing: Option<&str>,
) -> StoreResult<(PeerRow, Vec<PeerRow>)> {
    let existing = tx
        .query_row(
            &format!("SELECT {PEER_COLUMNS} FROM peers WHERE role = ?1 AND label = ?2"),
            params![Role::Host.as_str(), label],
            peer_row,
        )
        .optional()?;
    let keep = existing.as_ref().map(|row| to_i64(row.id)).unwrap_or(-1);
    let mut replaced = tx
        .prepare(&format!(
            "SELECT {PEER_COLUMNS} FROM peers WHERE pubkey = ?1 AND id != ?2"
        ))?
        .query_map(params![pubkey.as_slice(), keep], peer_row)?
        .collect::<Result<Vec<_>, _>>()?;
    for peer in &replaced {
        tx.execute("DELETE FROM peers WHERE id = ?1", [to_i64(peer.id)])?;
    }
    let row = match existing {
        Some(old) => {
            tx.execute(
                "UPDATE peers SET pubkey = ?1, pairing = ?2, push_token = NULL WHERE id = ?3",
                params![pubkey.as_slice(), pairing, to_i64(old.id)],
            )?;
            let row = PeerRow {
                pubkey: *pubkey,
                pairing: pairing.map(str::to_owned),
                push_token: None,
                ..old.clone()
            };
            replaced.push(old);
            row
        }
        None => {
            let now = now_unix();
            tx.execute(
                "INSERT INTO peers (role, label, pubkey, pairing, added_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![Role::Host.as_str(), label, pubkey.as_slice(), pairing, to_i64(now)],
            )?;
            PeerRow {
                id: to_u64(tx.last_insert_rowid()),
                role: Role::Host,
                label: label.to_owned(),
                pubkey: *pubkey,
                push_token: None,
                pairing: pairing.map(str::to_owned),
                added_at: now,
                last_seen: None,
            }
        }
    };
    tx.execute(
        "INSERT OR IGNORE INTO hosts (peer) VALUES (?1)",
        [to_i64(row.id)],
    )?;
    tx.commit()?;
    Ok((row, replaced))
}

const HOST_SELECT: &str = "SELECT p.id, p.role, p.label, p.pubkey, p.push_token, p.pairing, \
     p.added_at, p.last_seen, h.report, h.reported_at, h.environment, h.ssh, h.checks, \
     h.checked_at, h.limits \
     FROM hosts h JOIN peers p ON p.id = h.peer";

struct RawHost {
    peer: PeerRow,
    report: Option<String>,
    reported_at: Option<i64>,
    environment: String,
    ssh: Option<String>,
    checks: Option<String>,
    checked_at: Option<i64>,
    limits: String,
}

fn raw_host(row: &Row<'_>) -> rusqlite::Result<RawHost> {
    Ok(RawHost {
        peer: peer_row(row)?,
        report: row.get(8)?,
        reported_at: row.get(9)?,
        environment: row.get(10)?,
        ssh: row.get(11)?,
        checks: row.get(12)?,
        checked_at: row.get(13)?,
        limits: row.get(14)?,
    })
}

fn host_row(raw: RawHost) -> StoreResult<HostRow> {
    Ok(HostRow {
        peer: raw.peer,
        report: raw
            .report
            .map(|text| from_json(&text, "host report"))
            .transpose()?,
        reported_at: raw.reported_at.map(to_u64),
        environment: from_json(&raw.environment, "host environment")?,
        ssh: raw.ssh,
        checks: raw
            .checks
            .map(|text| from_json(&text, "host checks"))
            .transpose()?
            .unwrap_or_default(),
        checked_at: raw.checked_at.map(to_u64),
        limits: from_json(&raw.limits, "host limits")?,
    })
}

const CODESPACE_SELECT: &str = "SELECT c.id, c.workspace, w.name, c.host, p.label, c.peer, \
     c.state, c.cpus, c.memory_mib, c.instance, c.branch, c.base_branch, c.client_version, \
     c.agent, c.step, c.error, c.created_at, c.updated_at, c.preset, s.name, c.repositories, \
     c.first_message, c.stopped_at, c.active, c.active_at, c.metrics, \
     c.archive_size, c.archive_at, c.archive_error \
     FROM codespaces c JOIN workspaces w ON w.id = c.workspace JOIN peers p ON p.id = c.host \
     LEFT JOIN presets s ON s.id = c.preset";

type RawCodespace = (CodespaceRow, Option<String>, String, Option<String>);

fn raw_codespace(row: &Row<'_>) -> rusqlite::Result<RawCodespace> {
    let archive_size = row.get::<_, Option<i64>>(26)?.map(to_u64);
    let saved_at = row.get::<_, Option<i64>>(27)?.map(to_u64);
    let archive_error: Option<String> = row.get(28)?;
    Ok((
        CodespaceRow {
            id: row.get(0)?,
            workspace: to_u64(row.get(1)?),
            workspace_name: row.get(2)?,
            host: to_u64(row.get(3)?),
            host_name: row.get(4)?,
            preset: row.get::<_, Option<i64>>(18)?.map(to_u64),
            preset_name: row.get(19)?,
            peer: row.get::<_, Option<i64>>(5)?.map(to_u64),
            state: CodespaceState::parse(&row.get::<_, String>(6)?),
            cpus: u32::try_from(row.get::<_, i64>(7)?).unwrap_or_default(),
            memory_mib: to_u64(row.get(8)?),
            instance: row.get(9)?,
            branch: row.get(10)?,
            base_branch: row.get(11)?,
            repositories: Vec::new(),
            client_version: row.get(12)?,
            agent: None,
            step: row.get(14)?,
            error: row.get(15)?,
            first_message: row.get(21)?,
            created_at: to_u64(row.get(16)?),
            updated_at: to_u64(row.get(17)?),
            stopped_at: row.get::<_, Option<i64>>(22)?.map(to_u64),
            active: row.get::<_, Option<i64>>(23)?.map(|v| v != 0),
            active_at: row.get::<_, Option<i64>>(24)?.map(to_u64),
            metrics: None,
            archive: (saved_at.is_some() || archive_error.is_some()).then(|| ArchiveRow {
                size: archive_size.unwrap_or_default(),
                saved_at,
                error: archive_error,
            }),
        },
        row.get(13)?,
        row.get(20)?,
        row.get(25)?,
    ))
}

fn codespace_row(
    (mut row, agent, repositories, metrics): RawCodespace,
) -> StoreResult<CodespaceRow> {
    row.agent = agent
        .map(|text| from_json(&text, "codespace agent"))
        .transpose()?;
    row.repositories = from_json(&repositories, "codespace repositories")?;
    row.metrics = metrics
        .map(|text| from_json(&text, "codespace metrics"))
        .transpose()?;
    Ok(row)
}

fn repositories(conn: &rusqlite::Connection, workspace: u64) -> StoreResult<Vec<Repository>> {
    let rows = conn
        .prepare(
            "SELECT r.url, p.name, r.directory, r.base_branch FROM workspace_repositories r
             LEFT JOIN providers p ON p.id = r.provider
             WHERE r.workspace = ?1 ORDER BY r.position",
        )?
        .query_map([to_i64(workspace)], |row| {
            Ok(Repository {
                url: row.get(0)?,
                provider: row.get(1)?,
                directory: Some(row.get(2)?),
                base_branch: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn put_repositories(
    tx: &Transaction<'_>,
    workspace: i64,
    repositories: &[Repository],
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM workspace_repositories WHERE workspace = ?1",
        [workspace],
    )?;
    for (position, repository) in repositories.iter().enumerate() {
        let provider = match &repository.provider {
            Some(name) => Some(
                tx.query_row("SELECT id FROM providers WHERE name = ?1", [name], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
                .ok_or_else(|| StoreError::Conflict(format!("there is no provider {name}")))?,
            ),
            None => None,
        };
        tx.execute(
            "INSERT INTO workspace_repositories
                 (workspace, position, url, provider, directory, base_branch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                workspace,
                position as i64,
                repository.url,
                provider,
                repository.directory.as_deref().unwrap_or_default(),
                repository.base_branch
            ],
        )?;
    }
    Ok(())
}

fn open_codespaces(conn: &rusqlite::Connection, column: &str, id: u64) -> StoreResult<Vec<String>> {
    let ids = conn
        .prepare(&format!(
            "SELECT id FROM codespaces WHERE {column} = ?1 AND state != ?2 ORDER BY id"
        ))?
        .query_map(
            params![to_i64(id), CodespaceState::Closed.as_str()],
            |row| row.get(0),
        )?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(ids)
}

impl Store {
    pub fn hosts(&self) -> StoreResult<Vec<HostRow>> {
        let conn = self.conn();
        let rows = conn
            .prepare(&format!("{HOST_SELECT} ORDER BY p.label"))?
            .query_map([], raw_host)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(host_row).collect()
    }

    pub fn host(&self, peer: u64) -> StoreResult<Option<HostRow>> {
        let raw = self
            .conn()
            .query_row(
                &format!("{HOST_SELECT} WHERE h.peer = ?1"),
                [to_i64(peer)],
                raw_host,
            )
            .optional()?;
        raw.map(host_row).transpose()
    }

    pub fn host_by_name(&self, name: &str) -> StoreResult<Option<HostRow>> {
        let raw = self
            .conn()
            .query_row(
                &format!("{HOST_SELECT} WHERE p.label = ?1"),
                [name],
                raw_host,
            )
            .optional()?;
        raw.map(host_row).transpose()
    }

    pub fn set_host_report(&self, peer: u64, report: &HostReport) -> StoreResult<()> {
        let mut report = report.clone();
        let checks = std::mem::take(&mut report.checks);
        let now = to_i64(now_unix());
        let conn = self.conn();
        conn.execute(
            "UPDATE hosts SET report = ?1, reported_at = ?2 WHERE peer = ?3",
            params![to_json(&report), now, to_i64(peer)],
        )?;
        if !checks.is_empty() {
            drop(conn);
            self.set_host_checks(peer, &checks)?;
        }
        Ok(())
    }

    pub fn set_host_checks(&self, peer: u64, checks: &[HostCheck]) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "UPDATE hosts SET checks = ?1, checked_at = ?2 WHERE peer = ?3",
            params![to_json(&checks), to_i64(now_unix()), to_i64(peer)],
        )? > 0)
    }

    pub fn set_host_ssh(&self, peer: u64, ssh: Option<&str>) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "UPDATE hosts SET ssh = ?1 WHERE peer = ?2",
            params![ssh, to_i64(peer)],
        )? > 0)
    }

    pub fn set_host_limits(&self, peer: u64, limits: &HostLimits) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "UPDATE hosts SET limits = ?1 WHERE peer = ?2",
            params![to_json(limits), to_i64(peer)],
        )? > 0)
    }

    pub fn set_host_environment(&self, peer: u64, environment: &Environment) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "UPDATE hosts SET environment = ?1 WHERE peer = ?2",
            params![to_json(environment), to_i64(peer)],
        )? > 0)
    }

    pub fn remove_host(&self, peer: u64) -> StoreResult<Option<PeerRow>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let workspaces = tx
            .prepare("SELECT name FROM workspaces WHERE host = ?1 ORDER BY name")?
            .query_map([to_i64(peer)], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        if !workspaces.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the workspaces {} use this host",
                workspaces.join(", ")
            )));
        }
        let open = open_codespaces(&tx, "host", peer)?;
        if !open.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the codespaces {} run on this host",
                open.join(", ")
            )));
        }
        let volumes = tx
            .prepare("SELECT v.name FROM volume_hosts h JOIN volumes v ON v.id = h.volume WHERE h.host = ?1 ORDER BY v.name")?
            .query_map([to_i64(peer)], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !volumes.is_empty() {
            return Err(StoreError::Conflict(format!(
                "this host may hold the volumes {}; delete them first",
                volumes.join(", ")
            )));
        }
        tx.execute("DELETE FROM codespaces WHERE host = ?1", [to_i64(peer)])?;
        let removed = tx
            .query_row(
                &format!("DELETE FROM peers WHERE id = ?1 AND role = ?2 RETURNING {PEER_COLUMNS}"),
                params![to_i64(peer), Role::Host.as_str()],
                peer_row,
            )
            .optional()?;
        tx.commit()?;
        Ok(removed)
    }

    fn workspace_rows(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> StoreResult<Vec<WorkspaceRow>> {
        let conn = self.conn();
        let rows = conn
            .prepare(&format!(
                "SELECT w.id, w.name, w.host, p.label, w.created_at
                 FROM workspaces w JOIN peers p ON p.id = w.host {filter} ORDER BY w.name"
            ))?
            .query_map(args, |row| {
                Ok(WorkspaceRow {
                    id: to_u64(row.get(0)?),
                    name: row.get(1)?,
                    host: to_u64(row.get(2)?),
                    host_name: row.get(3)?,
                    repositories: Vec::new(),
                    volumes: Vec::new(),
                    created_at: to_u64(row.get(4)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|mut row| {
                row.repositories = repositories(&conn, row.id)?;
                row.volumes = workspace_volumes(&conn, row.id)?;
                Ok(row)
            })
            .collect()
    }

    pub fn workspaces(&self) -> StoreResult<Vec<WorkspaceRow>> {
        self.workspace_rows("", &[])
    }

    pub fn workspace_by_name(&self, name: &str) -> StoreResult<Option<WorkspaceRow>> {
        Ok(self
            .workspace_rows("WHERE w.name = ?1", &[&name])?
            .into_iter()
            .next())
    }

    pub fn create_workspace(
        &self,
        name: &str,
        host: u64,
        repositories: &[Repository],
        volumes: &[WorkspaceVolume],
    ) -> StoreResult<WorkspaceRow> {
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let taken: Option<i64> = tx
                .query_row("SELECT id FROM workspaces WHERE name = ?1", [name], |row| {
                    row.get(0)
                })
                .optional()?;
            if taken.is_some() {
                return Err(StoreError::Conflict(format!(
                    "a workspace has the name {name} already"
                )));
            }
            tx.execute(
                "INSERT INTO workspaces (name, host, created_at) VALUES (?1, ?2, ?3)",
                params![name, to_i64(host), to_i64(now_unix())],
            )?;
            let id = tx.last_insert_rowid();
            put_repositories(&tx, id, repositories)?;
            put_workspace_volumes(&tx, id, volumes)?;
            tx.commit()?;
        }
        self.workspace_by_name(name)?
            .ok_or(StoreError::Malformed("new workspace"))
    }

    pub fn update_workspace(
        &self,
        name: &str,
        host: Option<u64>,
        repositories: Option<&[Repository]>,
        volumes: Option<&[WorkspaceVolume]>,
    ) -> StoreResult<Option<WorkspaceRow>> {
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let Some(id) = tx
                .query_row("SELECT id FROM workspaces WHERE name = ?1", [name], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
            else {
                return Ok(None);
            };
            if let Some(host) = host {
                tx.execute(
                    "UPDATE workspaces SET host = ?1 WHERE id = ?2",
                    params![to_i64(host), id],
                )?;
            }
            if let Some(repositories) = repositories {
                put_repositories(&tx, id, repositories)?;
            }
            if let Some(volumes) = volumes {
                put_workspace_volumes(&tx, id, volumes)?;
            }
            tx.commit()?;
        }
        self.workspace_by_name(name)
    }

    pub fn delete_workspace(&self, name: &str) -> StoreResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(id) = tx
            .query_row("SELECT id FROM workspaces WHERE name = ?1", [name], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
        else {
            return Ok(false);
        };
        let open = open_codespaces(&tx, "workspace", to_u64(id))?;
        if !open.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the codespaces {} use this workspace; close them first",
                open.join(", ")
            )));
        }
        tx.execute("DELETE FROM codespaces WHERE workspace = ?1", [id])?;
        tx.execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    fn codespace_rows(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> StoreResult<Vec<CodespaceRow>> {
        let rows = self
            .conn()
            .prepare(&format!(
                "{CODESPACE_SELECT} {filter} ORDER BY c.created_at, c.id"
            ))?
            .query_map(args, raw_codespace)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(codespace_row).collect()
    }

    pub fn codespaces(&self) -> StoreResult<Vec<CodespaceRow>> {
        self.codespace_rows("", &[])
    }

    pub fn host_codespaces(&self, host: u64) -> StoreResult<Vec<CodespaceRow>> {
        self.codespace_rows("WHERE c.host = ?1", &[&to_i64(host)])
    }

    pub fn codespace(&self, id: &str) -> StoreResult<Option<CodespaceRow>> {
        Ok(self
            .codespace_rows("WHERE c.id = ?1", &[&id])?
            .into_iter()
            .next())
    }

    pub fn codespace_by_peer(&self, peer: u64) -> StoreResult<Option<CodespaceRow>> {
        Ok(self
            .codespace_rows("WHERE c.peer = ?1", &[&to_i64(peer)])?
            .into_iter()
            .next())
    }

    pub fn count_open_codespaces(&self, host: u64) -> StoreResult<u32> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM codespaces WHERE host = ?1 AND state != ?2",
            params![to_i64(host), CodespaceState::Closed.as_str()],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    pub fn count_running_codespaces(&self, host: u64) -> StoreResult<u32> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM codespaces WHERE host = ?1 AND state IN (?2, ?3, ?4)",
            params![
                to_i64(host),
                CodespaceState::Provisioning.as_str(),
                CodespaceState::Starting.as_str(),
                CodespaceState::Ready.as_str(),
            ],
            |row| row.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    pub fn insert_codespace(
        &self,
        row: &CodespaceRow,
        volumes: &[InstanceVolume],
    ) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO codespaces (id, workspace, host, peer, state, cpus, memory_mib, instance,
                 branch, base_branch, client_version, agent, step, error, created_at, updated_at,
                 preset, repositories, first_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                row.id,
                to_i64(row.workspace),
                to_i64(row.host),
                row.peer.map(to_i64),
                row.state.as_str(),
                i64::from(row.cpus),
                to_i64(row.memory_mib),
                row.instance,
                row.branch,
                row.base_branch,
                row.client_version,
                row.agent.as_ref().map(to_json),
                row.step,
                row.error,
                to_i64(row.created_at),
                to_i64(row.updated_at),
                row.preset.map(to_i64),
                to_json(&row.repositories),
                row.first_message,
            ],
        )?;
        put_codespace_volumes(&tx, &row.id, row.host, volumes)?;
        tx.commit()?;
        Ok(())
    }

    pub fn save_codespace(&self, row: &CodespaceRow) -> StoreResult<()> {
        let archive = row.archive.as_ref();
        self.conn().execute(
            "UPDATE codespaces SET peer = ?1, state = ?2, agent = ?3, step = ?4, error = ?5,
                 updated_at = ?6, stopped_at = ?8, active = ?9, active_at = ?10, metrics = ?11,
                 archive_size = ?12, archive_at = ?13, archive_error = ?14
             WHERE id = ?7",
            params![
                row.peer.map(to_i64),
                row.state.as_str(),
                row.agent.as_ref().map(to_json),
                row.step,
                row.error,
                to_i64(row.updated_at),
                row.id,
                row.stopped_at.map(to_i64),
                row.active.map(i64::from),
                row.active_at.map(to_i64),
                row.metrics.as_ref().map(to_json),
                archive.map(|a| to_i64(a.size)),
                archive.and_then(|a| a.saved_at).map(to_i64),
                archive.and_then(|a| a.error.as_deref()),
            ],
        )?;
        Ok(())
    }

    pub fn delete_closed_codespaces(
        &self,
        before: Option<u64>,
        archived_before: Option<u64>,
    ) -> StoreResult<Vec<String>> {
        let conn = self.conn();
        let ids = conn
            .prepare(
                "DELETE FROM codespaces WHERE state = ?1 AND (
                     (archive_at IS NULL AND ?2 IS NOT NULL AND updated_at < ?2)
                     OR (archive_at IS NOT NULL AND ?3 IS NOT NULL AND archive_at < ?3)
                 ) RETURNING id",
            )?
            .query_map(
                params![
                    CodespaceState::Closed.as_str(),
                    before.map(to_i64),
                    archived_before.map(to_i64)
                ],
                |row| row.get(0),
            )?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    pub fn mark_session_resumed(
        &self,
        codespace: &str,
        session: &str,
        by: &str,
    ) -> StoreResult<()> {
        self.conn().execute(
            "INSERT INTO resumed_sessions (codespace, session, resumed_by, resumed_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (codespace, session) DO UPDATE SET resumed_by = excluded.resumed_by,
                 resumed_at = excluded.resumed_at",
            params![codespace, session, by, to_i64(now_unix())],
        )?;
        Ok(())
    }

    pub fn resumed_sessions(&self, codespace: &str) -> StoreResult<Vec<String>> {
        let conn = self.conn();
        let ids = conn
            .prepare("SELECT session FROM resumed_sessions WHERE codespace = ?1")?
            .query_map([codespace], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    pub fn cancel_pairings_for(&self, target: &str) -> StoreResult<Vec<String>> {
        let conn = self.conn();
        let ids = conn
            .prepare("DELETE FROM pairings WHERE target = ?1 RETURNING id")?
            .query_map([target], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::seal::MasterKey;

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.unlock(&MasterKey::generate()).unwrap();
        store
    }

    fn codespace(id: &str, workspace: &WorkspaceRow) -> CodespaceRow {
        CodespaceRow {
            id: id.into(),
            workspace: workspace.id,
            workspace_name: workspace.name.clone(),
            host: workspace.host,
            host_name: workspace.host_name.clone(),
            preset: None,
            preset_name: None,
            peer: None,
            state: CodespaceState::Provisioning,
            cpus: 2,
            memory_mib: 4096,
            instance: format!("loom-{id}"),
            branch: format!("loom/{id}"),
            base_branch: None,
            repositories: workspace.repositories.clone(),
            client_version: Some("0.5.0".into()),
            agent: None,
            step: None,
            error: None,
            first_message: None,
            created_at: 1,
            updated_at: 1,
            stopped_at: None,
            active: None,
            active_at: None,
            metrics: None,
            archive: None,
        }
    }

    #[test]
    fn a_host_keeps_its_id_when_it_joins_again() {
        let store = store();
        let (host, replaced) = store
            .add_peer(Role::Host, "box", &[1; 32], Some("p1"))
            .unwrap();
        assert!(replaced.is_empty());
        let row = store.host(host.id).unwrap().unwrap();
        assert_eq!(row.peer, host);
        assert_eq!(row.environment, Environment::default());
        assert_eq!(store.host_by_name("box").unwrap().unwrap().peer.id, host.id);

        let (again, replaced) = store
            .add_peer(Role::Host, "box", &[2; 32], Some("p2"))
            .unwrap();
        assert_eq!(again.id, host.id);
        assert_eq!(again.pubkey, [2; 32]);
        assert_eq!(replaced, vec![host.clone()]);
        assert!(store.peer_by_key(&[1; 32]).unwrap().is_none());
        assert_eq!(store.hosts().unwrap().len(), 1);

        let report = HostReport {
            version: "0.5.0".into(),
            driver: "process".into(),
            cpus: 8,
            memory_mib: 16384,
            ..Default::default()
        };
        store.set_host_report(host.id, &report).unwrap();
        let mut environment = Environment::default();
        environment
            .variables
            .insert("HTTP_PROXY".into(), "http://proxy:3128".into());
        assert!(store.set_host_environment(host.id, &environment).unwrap());
        let row = store.host(host.id).unwrap().unwrap();
        assert_eq!(row.report, Some(report));
        assert!(row.reported_at.is_some());
        assert_eq!(row.environment, environment);
    }

    #[test]
    fn workspaces_and_codespaces_guard_their_deletes() {
        let store = store();
        let (host, _) = store.add_peer(Role::Host, "box", &[1; 32], None).unwrap();
        let repository = Repository {
            url: "https://github.com/acme/server.git".into(),
            provider: None,
            directory: Some("server".into()),
            base_branch: Some("main".into()),
        };
        let workspace = store
            .create_workspace("acme", host.id, std::slice::from_ref(&repository), &[])
            .unwrap();
        assert_eq!(workspace.repositories, vec![repository.clone()]);
        assert_eq!(workspace.host_name, "box");
        assert!(matches!(
            store.create_workspace("acme", host.id, &[], &[]),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.remove_host(host.id),
            Err(StoreError::Conflict(_))
        ));
        let two = [
            repository.clone(),
            Repository {
                url: "https://github.com/acme/client.git".into(),
                directory: Some("client".into()),
                ..repository.clone()
            },
        ];
        let changed = store
            .update_workspace("acme", Some(host.id), Some(&two), None)
            .unwrap()
            .unwrap();
        assert_eq!(changed.repositories, two.to_vec());
        assert!(
            store
                .update_workspace("none", None, None, None)
                .unwrap()
                .is_none()
        );
        let workspace = store
            .update_workspace("acme", None, Some(std::slice::from_ref(&repository)), None)
            .unwrap()
            .unwrap();
        assert_eq!(workspace.repositories, vec![repository.clone()]);

        let mut row = codespace("c1", &workspace);
        store.insert_codespace(&row, &[]).unwrap();
        assert_eq!(store.codespace("c1").unwrap().unwrap(), row);
        assert_eq!(store.count_open_codespaces(host.id).unwrap(), 1);
        assert!(matches!(
            store.delete_workspace("acme"),
            Err(StoreError::Conflict(_))
        ));

        let (peer, _) = store
            .add_peer(Role::Codespace, "c1", &[3; 32], None)
            .unwrap();
        row.peer = Some(peer.id);
        row.state = CodespaceState::Ready;
        row.agent = Some(AgentAddress {
            host_id: "ab".repeat(32),
            host_key: "key".into(),
            instance: Some("i1".into()),
        });
        row.step = Some("clone".into());
        row.updated_at = 5;
        store.save_codespace(&row).unwrap();
        assert_eq!(store.codespace_by_peer(peer.id).unwrap().unwrap(), row);
        assert_eq!(store.host_codespaces(host.id).unwrap(), vec![row.clone()]);

        store.remove_peer(peer.id).unwrap();
        assert_eq!(store.codespace("c1").unwrap().unwrap().peer, None);
        row.peer = None;
        row.state = CodespaceState::Closed;
        store.save_codespace(&row).unwrap();
        assert_eq!(store.count_open_codespaces(host.id).unwrap(), 0);
        assert!(store.delete_workspace("acme").unwrap());
        assert!(!store.delete_workspace("acme").unwrap());
        assert!(store.codespaces().unwrap().is_empty());
        assert_eq!(store.remove_host(host.id).unwrap().unwrap().id, host.id);
        assert!(store.hosts().unwrap().is_empty());
    }

    #[test]
    fn closed_codespaces_with_an_archive_stay_for_the_archive_retention() {
        let store = store();
        let (host, _) = store.add_peer(Role::Host, "box", &[1; 32], None).unwrap();
        let workspace = store.create_workspace("acme", host.id, &[], &[]).unwrap();
        for (id, archive) in [
            ("plain", None),
            (
                "kept",
                Some(ArchiveRow {
                    size: 10,
                    saved_at: Some(100),
                    error: None,
                }),
            ),
            (
                "failed",
                Some(ArchiveRow {
                    error: Some("too large".into()),
                    ..Default::default()
                }),
            ),
        ] {
            let mut row = codespace(id, &workspace);
            store.insert_codespace(&row, &[]).unwrap();
            row.state = CodespaceState::Closed;
            row.updated_at = 50;
            row.archive = archive;
            store.save_codespace(&row).unwrap();
            assert_eq!(store.codespace(id).unwrap().unwrap(), row);
        }
        let mut removed = store.delete_closed_codespaces(Some(60), Some(60)).unwrap();
        removed.sort();
        assert_eq!(removed, vec!["failed".to_string(), "plain".to_string()]);
        assert!(
            store
                .delete_closed_codespaces(None, Some(100))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .delete_closed_codespaces(Some(1000), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.delete_closed_codespaces(None, Some(101)).unwrap(),
            vec!["kept".to_string()]
        );
    }

    #[test]
    fn pairings_can_name_their_target() {
        let store = store();
        let (row, _) = store
            .create_pairing_for(Role::Codespace, Duration::from_secs(60), None, Some("c1"))
            .unwrap();
        assert_eq!(row.target.as_deref(), Some("c1"));
        let (taken, _) = store.take_pairing(&row.id).unwrap().unwrap();
        assert_eq!(taken.target.as_deref(), Some("c1"));
        let (row, _) = store
            .create_pairing_for(Role::Codespace, Duration::from_secs(60), None, Some("c2"))
            .unwrap();
        assert_eq!(store.cancel_pairings_for("c2").unwrap(), vec![row.id]);
        assert!(store.pairings().unwrap().is_empty());
    }
}
