use std::time::Duration;

use loom_oracle_proto::{
    ErrorCode, Event, HostCall, InstanceVolume, OracleError, Repository, Volume, VolumeInput,
    VolumeKind, WorkspaceCheck, WorkspaceCheckResult, WorkspaceVolume, check_mount_path,
    clone_paths, nested, valid_absolute_path, valid_name,
};

use super::{Oracle, bad_request, not_found};
use crate::store::VolumeRow;

const MAX_VOLUMES: usize = 20;
const MAX_SIZE_MIB: u64 = 16 * 1024 * 1024;
const VOLUME_TIMEOUT: Duration = Duration::from_secs(120);
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

fn unavailable(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Unavailable, message)
}

fn conflict(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Conflict, message)
}

fn volume(row: &VolumeRow) -> Volume {
    Volume {
        name: row.name.clone(),
        kind: row.kind,
        size_mib: row.size_mib,
        path: row.path.clone(),
        hosts: row.hosts.iter().map(|(_, name)| name.clone()).collect(),
        deleting: row.deleting,
        created_at: row.created_at,
    }
}

impl Oracle {
    pub fn volumes(&self) -> Result<Vec<Volume>, OracleError> {
        Ok(self.store.volumes()?.iter().map(volume).collect())
    }

    fn volume_row(&self, name: &str) -> Result<VolumeRow, OracleError> {
        self.store
            .volume_by_name(name)?
            .ok_or_else(|| not_found(format!("no volume has the name {name}")))
    }

    pub fn create_volume(
        &self,
        input: &VolumeInput,
        by: Option<u64>,
    ) -> Result<Volume, OracleError> {
        let name = &input.name;
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not a volume name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        match input.kind {
            VolumeKind::Incus => {
                if input.path.is_some() {
                    return Err(bad_request("an Incus volume has no host path"));
                }
                if input
                    .size_mib
                    .is_some_and(|size| size == 0 || size > MAX_SIZE_MIB)
                {
                    return Err(bad_request(format!(
                        "the size of a volume is 1MiB to {}TiB",
                        MAX_SIZE_MIB >> 20
                    )));
                }
            }
            VolumeKind::Directory => {
                if input.size_mib.is_some() {
                    return Err(bad_request("a host directory has no size"));
                }
                let path = input
                    .path
                    .as_deref()
                    .ok_or_else(|| bad_request("a host directory needs its path on the host"))?;
                if !valid_absolute_path(path) || path == "/" {
                    return Err(bad_request(format!(
                        "{path:?} is not a path of a directory on the host"
                    )));
                }
            }
            VolumeKind::Unknown => return Err(bad_request("the kind is incus or directory")),
        }
        let row =
            self.store
                .create_volume(name, input.kind, input.size_mib, input.path.as_deref())?;
        self.audit(
            by,
            "volume_created",
            &format!("volume {name} ({})", input.kind),
        );
        let volume = volume(&row);
        self.emit(Event::VolumeChanged {
            volume: volume.clone(),
        });
        Ok(volume)
    }

    pub async fn delete_volume(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        let _delete_guard = self.volume_delete_lock.lock().await;
        let mut row = {
            let _workspace_guard = self.workspace_lock.lock().await;
            let mut row = self.volume_row(name)?;
            self.store.begin_volume_delete(row.id)?;
            if !row.deleting {
                row.deleting = true;
                self.emit(Event::VolumeChanged {
                    volume: volume(&row),
                });
            }
            row
        };
        for (host, host_name) in &row.hosts {
            if !self.host_connected(*host) {
                return Err(unavailable(format!(
                    "the host {host_name} keeps the volume and is not connected"
                )));
            }
        }
        for (host, host_name) in std::mem::take(&mut row.hosts) {
            self.call_host(
                host,
                HostCall::DeleteVolume {
                    volume: name.to_owned(),
                },
                VOLUME_TIMEOUT,
            )
            .await
            .map_err(|err| {
                OracleError::new(
                    err.code,
                    format!(
                        "the host {host_name} cannot delete the volume: {}",
                        err.message
                    ),
                )
            })?;
            self.store.remove_volume_host(row.id, host)?;
            let changed = self.volume_row(name)?;
            self.emit(Event::VolumeChanged {
                volume: volume(&changed),
            });
        }
        self.store.delete_volume(row.id)?;
        self.audit(by, "volume_deleted", &format!("volume {name}"));
        self.emit(Event::VolumeRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    pub(super) fn resolve_volumes(
        &self,
        workspace: &str,
        repositories: &[Repository],
        inputs: &[WorkspaceVolume],
    ) -> Result<Vec<WorkspaceVolume>, OracleError> {
        if inputs.len() > MAX_VOLUMES {
            return Err(bad_request(format!(
                "a workspace has up to {MAX_VOLUMES} volumes"
            )));
        }
        let directories: Vec<String> = repositories
            .iter()
            .filter_map(|r| r.directory.clone())
            .collect();
        let clones = clone_paths(workspace, &directories);
        let mut resolved: Vec<WorkspaceVolume> = Vec::new();
        for input in inputs {
            if self.volume_row(&input.volume)?.deleting {
                return Err(conflict(format!(
                    "the volume {} is being deleted",
                    input.volume
                )));
            }
            check_mount_path(&input.path, &clones).map_err(bad_request)?;
            if resolved.iter().any(|v| v.volume == input.volume) {
                return Err(bad_request(format!(
                    "the workspace mounts the volume {} twice",
                    input.volume
                )));
            }
            if let Some(other) = resolved.iter().find(|v| nested(&v.path, &input.path)) {
                return Err(bad_request(format!(
                    "the mount paths {} and {} overlap",
                    other.path, input.path
                )));
            }
            resolved.push(input.clone());
        }
        Ok(resolved)
    }

    pub(super) async fn check_volumes(
        &self,
        host: u64,
        host_name: &str,
        volumes: &[WorkspaceVolume],
        required: bool,
    ) -> Result<(), OracleError> {
        if volumes.is_empty() {
            return Ok(());
        }
        let mut directories: Vec<_> = self
            .instance_volumes(volumes)?
            .into_iter()
            .filter_map(|mount| mount.source)
            .collect();
        directories.sort_unstable();
        directories.dedup();
        if !self.host_connected(host) {
            return if required {
                Err(unavailable(format!(
                    "the host {host_name} is not connected"
                )))
            } else {
                Ok(())
            };
        }
        let value = self
            .call_host(
                host,
                HostCall::CheckWorkspace(WorkspaceCheck {
                    providers: Vec::new(),
                    directories: directories.clone(),
                }),
                CHECK_TIMEOUT,
            )
            .await?;
        let checked: WorkspaceCheckResult = serde_json::from_value(value).map_err(|err| {
            OracleError::new(
                ErrorCode::Internal,
                format!("the host sent a malformed check: {err}"),
            )
        })?;
        if !checked.volumes {
            return Err(unavailable(format!(
                "the host {host_name} does not support volumes; update loom-host"
            )));
        }
        for path in directories {
            let mut results = checked.directories.iter().filter(|d| d.name == path);
            let result = results.next().filter(|_| results.next().is_none());
            let Some(result) = result else {
                return Err(unavailable(format!(
                    "the host {host_name} did not confirm the directory {path}"
                )));
            };
            if let Some(error) = &result.error {
                return Err(bad_request(format!(
                    "the host {host_name} cannot give the directory {path}: {error}"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn instance_volumes(
        &self,
        volumes: &[WorkspaceVolume],
    ) -> Result<Vec<InstanceVolume>, OracleError> {
        let mut result = Vec::new();
        for mount in volumes {
            let row = self.volume_row(&mount.volume)?;
            if row.deleting {
                return Err(conflict(format!(
                    "the volume {} is being deleted",
                    row.name
                )));
            }
            result.push(InstanceVolume {
                name: row.name.clone(),
                kind: row.kind,
                size_mib: row.size_mib,
                source: row.path.clone(),
                path: mount.path.clone(),
                read_only: mount.read_only,
            });
        }
        Ok(result)
    }

    pub(super) fn volume_hosts_changed(&self, volumes: &[InstanceVolume]) {
        for mount in volumes.iter().filter(|v| v.kind == VolumeKind::Incus) {
            if let Ok(Some(row)) = self.store.volume_by_name(&mount.name) {
                self.emit(Event::VolumeChanged {
                    volume: volume(&row),
                });
            }
        }
    }
}
