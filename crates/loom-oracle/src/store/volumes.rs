use loom_oracle_proto::{CodespaceState, InstanceVolume, VolumeKind, WorkspaceVolume};
use rusqlite::{OptionalExtension, Row, Transaction, params};

use super::{Store, StoreError, StoreResult, now_unix, to_i64, to_u64};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRow {
    pub id: u64,
    pub name: String,
    pub kind: VolumeKind,
    pub size_mib: Option<u64>,
    pub path: Option<String>,
    pub hosts: Vec<(u64, String)>,
    pub deleting: bool,
    pub created_at: u64,
}

fn volume_row(row: &Row<'_>) -> rusqlite::Result<VolumeRow> {
    Ok(VolumeRow {
        id: to_u64(row.get(0)?),
        name: row.get(1)?,
        kind: VolumeKind::parse(&row.get::<_, String>(2)?),
        size_mib: row.get::<_, Option<i64>>(3)?.map(to_u64),
        path: row.get(4)?,
        hosts: Vec::new(),
        deleting: row.get(5)?,
        created_at: to_u64(row.get(6)?),
    })
}

pub(super) fn workspace_volumes(
    conn: &rusqlite::Connection,
    workspace: u64,
) -> StoreResult<Vec<WorkspaceVolume>> {
    let rows = conn
        .prepare(
            "SELECT v.name, w.path, w.read_only FROM workspace_volumes w
             JOIN volumes v ON v.id = w.volume WHERE w.workspace = ?1 ORDER BY w.position",
        )?
        .query_map([to_i64(workspace)], |row| {
            Ok(WorkspaceVolume {
                volume: row.get(0)?,
                path: row.get(1)?,
                read_only: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn usable_volume(tx: &Transaction<'_>, name: &str) -> StoreResult<i64> {
    tx.query_row(
        "SELECT id FROM volumes WHERE name = ?1 AND deleting = 0",
        [name],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| StoreError::Conflict(format!("the volume {name} is missing or being deleted")))
}

pub(super) fn put_workspace_volumes(
    tx: &Transaction<'_>,
    workspace: i64,
    volumes: &[WorkspaceVolume],
) -> StoreResult<()> {
    tx.execute(
        "DELETE FROM workspace_volumes WHERE workspace = ?1",
        [workspace],
    )?;
    for (position, volume) in volumes.iter().enumerate() {
        let id = usable_volume(tx, &volume.volume)?;
        tx.execute(
            "INSERT INTO workspace_volumes (workspace, position, volume, path, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                workspace,
                position as i64,
                id,
                volume.path,
                volume.read_only
            ],
        )?;
    }
    Ok(())
}

pub(super) fn put_codespace_volumes(
    tx: &Transaction<'_>,
    codespace: &str,
    host: u64,
    volumes: &[InstanceVolume],
) -> StoreResult<()> {
    for volume in volumes {
        let id = usable_volume(tx, &volume.name)?;
        tx.execute(
            "INSERT INTO codespace_volumes (codespace, volume) VALUES (?1, ?2)",
            params![codespace, id],
        )?;
        if volume.kind == VolumeKind::Incus {
            tx.execute(
                "INSERT INTO volume_hosts (volume, host) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                params![id, to_i64(host)],
            )?;
        }
    }
    Ok(())
}

impl Store {
    fn volume_rows(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> StoreResult<Vec<VolumeRow>> {
        let conn = self.conn();
        let mut rows = conn
            .prepare(&format!(
                "SELECT id, name, kind, size_mib, path, deleting, created_at FROM volumes {filter} ORDER BY name"
            ))?
            .query_map(args, volume_row)?
            .collect::<Result<Vec<_>, _>>()?;
        for row in &mut rows {
            row.hosts = conn
                .prepare(
                    "SELECT p.id, p.label FROM volume_hosts h JOIN peers p ON p.id = h.host
                     WHERE h.volume = ?1 ORDER BY p.label",
                )?
                .query_map([to_i64(row.id)], |r| Ok((to_u64(r.get(0)?), r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
        }
        Ok(rows)
    }

    pub fn volumes(&self) -> StoreResult<Vec<VolumeRow>> {
        self.volume_rows("", &[])
    }

    pub fn volume_by_name(&self, name: &str) -> StoreResult<Option<VolumeRow>> {
        Ok(self
            .volume_rows("WHERE name = ?1", &[&name])?
            .into_iter()
            .next())
    }

    pub fn create_volume(
        &self,
        name: &str,
        kind: VolumeKind,
        size_mib: Option<u64>,
        path: Option<&str>,
    ) -> StoreResult<VolumeRow> {
        let inserted = self.conn().execute(
            "INSERT INTO volumes (name, kind, size_mib, path, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (name) DO NOTHING",
            params![name, kind.as_str(), size_mib.map(to_i64), path, to_i64(now_unix())],
        )?;
        if inserted == 0 {
            return Err(StoreError::Conflict(format!(
                "a volume has the name {name} already"
            )));
        }
        self.volume_by_name(name)?
            .ok_or(StoreError::Malformed("new volume"))
    }

    pub fn begin_volume_delete(&self, id: u64) -> StoreResult<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let workspaces = tx
            .prepare(
                "SELECT DISTINCT w.name FROM workspace_volumes v JOIN workspaces w ON w.id = v.workspace
                 WHERE v.volume = ?1 ORDER BY w.name",
            )?
            .query_map([to_i64(id)], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !workspaces.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the workspaces {} use this volume",
                workspaces.join(", ")
            )));
        }
        let codespaces = tx
            .prepare(
                "SELECT c.id FROM codespace_volumes v JOIN codespaces c ON c.id = v.codespace
                 WHERE v.volume = ?1 AND c.state != ?2 ORDER BY c.id",
            )?
            .query_map(params![to_i64(id), CodespaceState::Closed.as_str()], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if !codespaces.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the codespaces {} use this volume; close them first",
                codespaces.join(", ")
            )));
        }
        tx.execute(
            "UPDATE volumes SET deleting = 1 WHERE id = ?1",
            [to_i64(id)],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_volume_host(&self, volume: u64, host: u64) -> StoreResult<()> {
        self.conn().execute(
            "DELETE FROM volume_hosts WHERE volume = ?1 AND host = ?2",
            params![to_i64(volume), to_i64(host)],
        )?;
        Ok(())
    }

    pub fn delete_volume(&self, id: u64) -> StoreResult<()> {
        let deleted = self.conn().execute(
            "DELETE FROM volumes WHERE id = ?1 AND deleting = 1
             AND NOT EXISTS (SELECT 1 FROM volume_hosts WHERE volume = ?1)",
            [to_i64(id)],
        )?;
        if deleted == 0 {
            return Err(StoreError::Conflict(
                "the volume is not ready to delete".into(),
            ));
        }
        Ok(())
    }
}
