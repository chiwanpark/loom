use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use loom_oracle_proto::{
    ArchiveChunk, ArchiveFile, ArchiveFiles, ArchivePage, CodespaceArchive, CodespaceState,
    ErrorCode, HostCall, OracleError, valid_archive_file,
};
use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest, Sha256};

use super::codespaces::conflict;
use super::{Oracle, bad_request, not_found};
use crate::store::{ArchiveRow, CodespaceRow, now_unix};

const FILES_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MAX_PAGE: usize = 1024 * 1024;
const PARTIAL: &str = ".partial";

fn internal(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Internal, message)
}

fn io_error(what: &str, err: std::io::Error) -> OracleError {
    internal(format!("{what}: {err}"))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn archive_view(row: &CodespaceRow, retention: u64) -> Option<CodespaceArchive> {
    let archive = row.archive.as_ref()?;
    Some(CodespaceArchive {
        size: archive.size,
        saved_at: archive.saved_at,
        error: archive.error.clone(),
        delete_at: archive
            .saved_at
            .filter(|_| retention > 0)
            .map(|at| at.saturating_add(retention)),
    })
}

fn merge(dir: &Path) -> Result<u64, OracleError> {
    let entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| io_error("cannot read the archive", err))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    for path in entries
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "sqlite"))
    {
        let conn = Connection::open(path)
            .map_err(|err| internal(format!("cannot open a session store: {err}")))?;
        let checked = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .and_then(|()| conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(())))
            .and_then(|()| conn.query_row("SELECT count(*) FROM events", [], |_| Ok(())));
        if let Err(err) = checked {
            return Err(internal(format!(
                "{} is not a session store: {err}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )));
        }
    }
    let mut size = 0;
    for entry in std::fs::read_dir(dir).map_err(|err| io_error("cannot read the archive", err))? {
        let entry = entry.map_err(|err| io_error("cannot read the archive", err))?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "sqlite") {
            size += entry.metadata().map(|m| m.len()).unwrap_or_default();
        } else {
            std::fs::remove_file(&path).map_err(|err| io_error("cannot clean the archive", err))?;
        }
    }
    Ok(size)
}

fn read_page(
    dir: &Path,
    session: &str,
    after: Option<u64>,
) -> Result<Option<ArchivePage>, OracleError> {
    let mut stores: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| io_error("cannot read the archive", err))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "sqlite"))
        .collect();
    stores.sort();
    let sqlite = |err: rusqlite::Error| internal(format!("cannot read the archive: {err}"));
    for path in stores {
        let conn =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sqlite)?;
        let known: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1)
                     OR EXISTS (SELECT 1 FROM events WHERE session_id = ?1)",
                [session],
                |row| row.get(0),
            )
            .map_err(sqlite)?;
        if !known {
            continue;
        }
        let after = after.map_or(-1, |seq| i64::try_from(seq).unwrap_or(i64::MAX));
        let mut statement = conn
            .prepare(
                "SELECT seq, event FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(sqlite)?;
        let mut rows = statement.query(params![session, after]).map_err(sqlite)?;
        let mut page = ArchivePage::default();
        let mut bytes = 0;
        while let Some(row) = rows.next().map_err(sqlite)? {
            if bytes >= MAX_PAGE {
                page.more = true;
                break;
            }
            let seq: i64 = row.get(0).map_err(sqlite)?;
            let event: Vec<u8> = row.get(1).map_err(sqlite)?;
            bytes += event.len();
            page.events.push(STANDARD.encode(event));
            page.last = u64::try_from(seq).ok();
        }
        return Ok(Some(page));
    }
    Ok(None)
}

impl Oracle {
    pub(super) fn archive_dir(&self, id: &str) -> Option<PathBuf> {
        valid_id(id)
            .then(|| self.archive_root.as_ref().map(|root| root.join(id)))
            .flatten()
    }

    pub(super) fn remove_archive(&self, id: &str) {
        let Some(dir) = self.archive_dir(id) else {
            return;
        };
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(codespace = %id, "deleted the session archive"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(codespace = %id, error = %err, "cannot delete the session archive");
            }
        }
    }

    pub(super) fn clean_archives(&self) {
        let Some(root) = &self.archive_root else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let keep = !name.ends_with(PARTIAL)
                && self
                    .store
                    .codespace(&name)
                    .is_ok_and(|row| row.is_some_and(|row| row.archive.is_some()));
            if !keep {
                tracing::info!(entry = %name, "removing an unused entry of the session archive");
                let path = entry.path();
                let removed = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(err) = removed {
                    tracing::warn!(entry = %name, error = %err, "cannot remove an entry of the session archive");
                }
            }
        }
    }

    fn wants_archive(&self, row: &CodespaceRow) -> bool {
        self.lifecycle.archive
            && self.archive_root.is_some()
            && row.state == CodespaceState::Closing
            && row.archive.is_none()
    }

    pub(super) async fn archive_before_delete(
        self: &Arc<Self>,
        row: &CodespaceRow,
    ) -> Result<(), OracleError> {
        if !self.wants_archive(row) {
            return Ok(());
        }
        let archive = match self.copy_archive(row).await {
            Ok(None) => return Ok(()),
            Ok(Some(size)) => {
                tracing::info!(codespace = %row.id, size, "saved the session archive");
                ArchiveRow {
                    size,
                    saved_at: Some(now_unix()),
                    error: None,
                }
            }
            Err(err) if err.code == ErrorCode::Unavailable => return Err(err),
            Err(err) => {
                tracing::warn!(codespace = %row.id, error = %err.message, "cannot save the session archive");
                ArchiveRow {
                    error: Some(err.message),
                    ..Default::default()
                }
            }
        };
        self.update_codespace(&row.id, |row| {
            row.archive = Some(archive);
            true
        })?;
        Ok(())
    }

    async fn copy_archive(
        self: &Arc<Self>,
        row: &CodespaceRow,
    ) -> Result<Option<u64>, OracleError> {
        let dir = self
            .archive_dir(&row.id)
            .ok_or_else(|| internal("the codespace has no archive directory"))?;
        let stop = HostCall::StopInstance {
            codespace: row.id.clone(),
            instance: row.instance.clone(),
        };
        if let Err(err) = self.call_host(row.host, stop, STOP_TIMEOUT).await {
            if err.code == ErrorCode::Unavailable {
                return Err(err);
            }
            tracing::debug!(codespace = %row.id, error = %err.message, "cannot stop the instance before the archive");
        }
        let call = HostCall::ArchiveFiles {
            codespace: row.id.clone(),
            instance: row.instance.clone(),
            max_size: Some(self.lifecycle.archive_max_size.as_u64()),
        };
        let value = self.call_host(row.host, call, FILES_TIMEOUT).await?;
        let files: ArchiveFiles = serde_json::from_value(value)
            .map_err(|err| internal(format!("the host sent a bad file list: {err}")))?;
        if files
            .files
            .iter()
            .any(|file| !valid_archive_file(&file.name))
        {
            return Err(internal("the host sent a file name that is not valid"));
        }
        if files.files.is_empty() {
            return Ok(None);
        }
        let total: u64 = files.files.iter().map(|file| file.size).sum();
        let max = self.lifecycle.archive_max_size.as_u64();
        if total > max {
            return Err(bad_request(format!(
                "the sessions have {total} bytes, more than archive_max_size ({max} bytes)"
            )));
        }
        let partial = dir.with_file_name(format!("{}{PARTIAL}", row.id));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial)
            .map_err(|err| io_error("cannot make the archive directory", err))?;
        let result = self.download(row, &files.files, &partial).await;
        let result = match result {
            Ok(()) => {
                let merged = partial.clone();
                tokio::task::spawn_blocking(move || merge(&merged))
                    .await
                    .map_err(|err| internal(err.to_string()))
                    .and_then(|size| size)
            }
            Err(err) => Err(err),
        };
        let size = match result {
            Ok(size) => size,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&partial);
                return Err(err);
            }
        };
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&partial, &dir).map_err(|err| io_error("cannot keep the archive", err))?;
        Ok(Some(size))
    }

    async fn download(
        self: &Arc<Self>,
        row: &CodespaceRow,
        files: &[ArchiveFile],
        dir: &Path,
    ) -> Result<(), OracleError> {
        use std::io::Write;
        for file in files {
            let path = dir.join(&file.name);
            let mut out = std::fs::File::create(&path)
                .map_err(|err| io_error("cannot write the archive", err))?;
            let mut hash = Sha256::new();
            let mut offset = 0;
            while offset < file.size {
                let call = HostCall::ArchiveChunk {
                    codespace: row.id.clone(),
                    name: file.name.clone(),
                    offset,
                };
                let value = self.call_host(row.host, call, CHUNK_TIMEOUT).await?;
                let chunk: ArchiveChunk = serde_json::from_value(value)
                    .map_err(|err| internal(format!("the host sent a bad part: {err}")))?;
                let data = STANDARD
                    .decode(chunk.data)
                    .map_err(|err| internal(format!("the host sent a bad part: {err}")))?;
                if data.is_empty() {
                    break;
                }
                hash.update(&data);
                out.write_all(&data)
                    .map_err(|err| io_error("cannot write the archive", err))?;
                offset += data.len() as u64;
            }
            out.sync_all()
                .map_err(|err| io_error("cannot write the archive", err))?;
            if offset != file.size || hex(&hash.finalize()) != file.sha256 {
                return Err(internal(format!(
                    "the copy of {} does not match its size or checksum",
                    file.name
                )));
            }
        }
        Ok(())
    }

    pub fn archive_events(
        &self,
        codespace: &str,
        session: &str,
        after: Option<u64>,
    ) -> Result<ArchivePage, OracleError> {
        let row = self
            .store
            .codespace(codespace)?
            .ok_or_else(|| not_found(format!("no codespace has the ID {codespace}")))?;
        if row.archive.as_ref().and_then(|a| a.saved_at).is_none() {
            return Err(not_found(format!(
                "the codespace {codespace} has no session archive"
            )));
        }
        let dir = self
            .archive_dir(codespace)
            .ok_or_else(|| not_found("the oracle keeps no session archive"))?;
        read_page(&dir, session, after)?.ok_or_else(|| {
            not_found(format!(
                "the archive of {codespace} has no session {session}"
            ))
        })
    }

    pub fn delete_archive(&self, codespace: &str, by: Option<u64>) -> Result<(), OracleError> {
        let row = self
            .store
            .codespace(codespace)?
            .ok_or_else(|| not_found(format!("no codespace has the ID {codespace}")))?;
        if row.state != CodespaceState::Closed {
            return Err(conflict(format!("the codespace {codespace} is not closed")));
        }
        if row.archive.is_none() {
            return Err(not_found(format!(
                "the codespace {codespace} has no session archive"
            )));
        }
        self.remove_archive(codespace);
        self.change_codespace(codespace, false, |row| {
            row.archive = None;
            true
        })?;
        self.audit(by, "archive_deleted", &format!("codespace {codespace}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_codespace_ids_name_archive_directories() {
        assert!(valid_id("k3x9-abc"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id(""));
        assert!(!valid_id("A"));
    }

    #[test]
    fn the_merge_keeps_one_database_with_the_events_of_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0a1b.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY);
             CREATE TABLE events (session_id TEXT, seq INTEGER, event BLOB);
             INSERT INTO sessions VALUES ('s1');
             INSERT INTO events VALUES ('s1', 0, x'0102'), ('s1', 1, x'03');",
        )
        .unwrap();
        let wal = dir.path().join("0a1b.sqlite-wal");
        let copy = tempfile::tempdir().unwrap();
        std::fs::copy(&path, copy.path().join("0a1b.sqlite")).unwrap();
        std::fs::copy(&wal, copy.path().join("0a1b.sqlite-wal")).unwrap();
        drop(conn);

        let size = merge(copy.path()).unwrap();
        let names: Vec<String> = std::fs::read_dir(copy.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["0a1b.sqlite".to_string()]);
        assert!(size > 0);

        let page = read_page(copy.path(), "s1", None).unwrap().unwrap();
        assert_eq!(
            page.events,
            vec![STANDARD.encode([1, 2]), STANDARD.encode([3])]
        );
        assert!(!page.more);
        let rest = read_page(copy.path(), "s1", Some(0)).unwrap().unwrap();
        assert_eq!(rest.events, vec![STANDARD.encode([3])]);
        assert!(read_page(copy.path(), "other", None).unwrap().is_none());
    }

    #[test]
    fn a_file_that_is_not_a_session_store_fails_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("0a.sqlite"), b"not sqlite").unwrap();
        assert!(merge(dir.path()).is_err());
    }
}
