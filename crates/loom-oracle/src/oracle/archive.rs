use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use loom_oracle_proto::{
    ARCHIVE_CHUNK, ArchiveChunk, ArchiveFile, ArchiveFiles, ArchivePage, ArchivedSession,
    ArchivedSessionPart, CodespaceArchive, CodespaceState, ErrorCode, HostCall, OracleError,
    valid_archive_file,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::{Oracle, bad_request, not_found};
use crate::store::{ArchiveRow, CodespaceRow, now_unix};

const FILES_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const STOP_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MAX_PAGE: usize = 1024 * 1024;
const PARTIAL: &str = ".partial";
const TITLE_CHARS: usize = 80;

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

fn sqlite(err: rusqlite::Error) -> OracleError {
    internal(format!("cannot read the archive: {err}"))
}

fn stores(dir: &Path) -> Result<Vec<PathBuf>, OracleError> {
    let mut stores: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| io_error("cannot read the archive", err))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "sqlite"))
        .collect();
    stores.sort();
    Ok(stores)
}

fn open_store(path: &Path) -> Result<Connection, OracleError> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sqlite)
}

fn store_with(dir: &Path, session: &str) -> Result<Option<Connection>, OracleError> {
    for path in stores(dir)? {
        let conn = open_store(&path)?;
        let known: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1)
                     OR EXISTS (SELECT 1 FROM events WHERE session_id = ?1)",
                [session],
                |row| row.get(0),
            )
            .map_err(sqlite)?;
        if known {
            return Ok(Some(conn));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredSession {
    id: String,
    title: Option<String>,
    model: Option<String>,
    created_at: u64,
    updated_at: u64,
}

fn first_line(history: &[serde_json::Value]) -> Option<String> {
    history
        .iter()
        .filter(|message| message.get("role").and_then(|r| r.as_str()) == Some("user"))
        .find_map(|message| {
            message.get("content")?.as_array()?.iter().find_map(|part| {
                let text = part.get("text")?.as_str()?;
                let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
                Some(line.chars().take(TITLE_CHARS).collect())
            })
        })
}

fn stored_session(
    id: String,
    created_at: i64,
    updated_at: i64,
    state: &str,
) -> Option<StoredSession> {
    let state: serde_json::Value = serde_json::from_str(state).ok()?;
    let list = |key: &str| {
        state
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let history = list("history");
    if history.is_empty() && list("pending").is_empty() {
        return None;
    }
    let text = |key: &str| {
        state
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Some(StoredSession {
        id,
        title: text("title").or_else(|| first_line(&history)),
        model: text("model"),
        created_at: u64::try_from(created_at).unwrap_or_default(),
        updated_at: u64::try_from(updated_at).unwrap_or_default(),
    })
}

fn read_sessions(dir: &Path) -> Result<Vec<StoredSession>, OracleError> {
    let mut sessions = Vec::new();
    for path in stores(dir)? {
        let conn = open_store(&path)?;
        let mut statement = conn
            .prepare("SELECT id, created_at, updated_at, state FROM sessions")
            .map_err(sqlite)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(sqlite)?;
        for row in rows {
            let (id, created_at, updated_at, state) = row.map_err(sqlite)?;
            sessions.extend(stored_session(id, created_at, updated_at, &state));
        }
    }
    Ok(sessions)
}

fn read_page(
    dir: &Path,
    session: &str,
    after: Option<u64>,
) -> Result<Option<ArchivePage>, OracleError> {
    let Some(conn) = store_with(dir, session)? else {
        return Ok(None);
    };
    let after = after.map_or(-1, |seq| i64::try_from(seq).unwrap_or(i64::MAX));
    let mut statement = conn
        .prepare("SELECT seq, event FROM events WHERE session_id = ?1 AND seq > ?2 ORDER BY seq")
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
    Ok(Some(page))
}

fn read_part(
    dir: &Path,
    session: &str,
    offset: u64,
) -> Result<Option<(StoredSession, ArchivedSessionPart)>, OracleError> {
    let Some(conn) = store_with(dir, session)? else {
        return Ok(None);
    };
    let row = conn
        .query_row(
            "SELECT created_at, updated_at, state FROM sessions WHERE id = ?1",
            [session],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite)?;
    let Some((created_at, updated_at, state)) = row else {
        return Ok(None);
    };
    let Some(stored) = stored_session(session.to_owned(), created_at, updated_at, &state) else {
        return Ok(None);
    };
    let tasks: Option<String> = conn
        .query_row(
            "SELECT list FROM tasks WHERE session_id = ?1",
            [session],
            |row| row.get(0),
        )
        .optional()
        .or_else(|err| match err {
            rusqlite::Error::SqliteFailure(..) => Ok(None),
            other => Err(other),
        })
        .map_err(sqlite)?;
    let bytes = state.as_bytes();
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = start.saturating_add(ARCHIVE_CHUNK).min(bytes.len());
    let part = ArchivedSessionPart {
        session: ArchivedSession::default(),
        state: STANDARD.encode(&bytes[start..end]),
        size: bytes.len() as u64,
        tasks,
    };
    Ok(Some((stored, part)))
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

    fn has_archive(row: &CodespaceRow) -> bool {
        row.state == CodespaceState::Closed
            && row.archive.as_ref().is_some_and(|a| a.saved_at.is_some())
    }

    fn workspace_archive(
        &self,
        peer: u64,
        codespace: &str,
    ) -> Result<(CodespaceRow, CodespaceRow, PathBuf), OracleError> {
        let caller = self.codespace_of_peer(peer)?;
        let missing = || {
            not_found(format!(
                "no closed codespace of this workspace has the ID {codespace}"
            ))
        };
        let row = self.store.codespace(codespace)?.ok_or_else(missing)?;
        if row.workspace != caller.workspace || !Self::has_archive(&row) {
            return Err(missing());
        }
        let dir = self
            .archive_dir(codespace)
            .ok_or_else(|| not_found("the oracle keeps no session archive"))?;
        Ok((caller, row, dir))
    }

    pub fn archived_sessions(&self, peer: u64) -> Result<Vec<ArchivedSession>, OracleError> {
        let caller = self.codespace_of_peer(peer)?;
        let mut sessions = Vec::new();
        for row in self.store.codespaces()? {
            if row.workspace != caller.workspace || row.id == caller.id || !Self::has_archive(&row)
            {
                continue;
            }
            let Some(dir) = self.archive_dir(&row.id) else {
                continue;
            };
            let stored = match read_sessions(&dir) {
                Ok(stored) => stored,
                Err(err) => {
                    tracing::warn!(codespace = %row.id, error = %err.message, "cannot read the session archive");
                    continue;
                }
            };
            let resumed = self.store.resumed_sessions(&row.id)?;
            sessions.extend(
                stored
                    .into_iter()
                    .filter(|session| !resumed.contains(&session.id))
                    .map(|session| archived(&row, session)),
            );
        }
        sessions.sort_by(|a, b| (b.updated_at, &b.id).cmp(&(a.updated_at, &a.id)));
        Ok(sessions)
    }

    pub fn archived_session(
        &self,
        peer: u64,
        codespace: &str,
        session: &str,
        offset: u64,
    ) -> Result<ArchivedSessionPart, OracleError> {
        let (_, row, dir) = self.workspace_archive(peer, codespace)?;
        let (stored, mut part) = read_part(&dir, session, offset)?.ok_or_else(|| {
            not_found(format!(
                "the archive of {codespace} has no session {session}"
            ))
        })?;
        part.session = archived(&row, stored);
        Ok(part)
    }

    pub fn archive_events(
        &self,
        peer: u64,
        codespace: &str,
        session: &str,
        after: Option<u64>,
    ) -> Result<ArchivePage, OracleError> {
        let (_, _, dir) = self.workspace_archive(peer, codespace)?;
        read_page(&dir, session, after)?.ok_or_else(|| {
            not_found(format!(
                "the archive of {codespace} has no session {session}"
            ))
        })
    }

    pub fn mark_session_resumed(
        &self,
        peer: u64,
        codespace: &str,
        session: &str,
    ) -> Result<(), OracleError> {
        let (caller, _, dir) = self.workspace_archive(peer, codespace)?;
        if store_with(&dir, session)?.is_none() {
            return Err(not_found(format!(
                "the archive of {codespace} has no session {session}"
            )));
        }
        self.store
            .mark_session_resumed(codespace, session, &caller.id)?;
        self.audit(
            Some(peer),
            "session_resumed",
            &format!(
                "session {session} of codespace {codespace} in codespace {}",
                caller.id
            ),
        );
        Ok(())
    }
}

fn archived(row: &CodespaceRow, session: StoredSession) -> ArchivedSession {
    ArchivedSession {
        codespace: row.id.clone(),
        branch: row.branch.clone(),
        id: session.id,
        title: session.title,
        model: session.model,
        created_at: session.created_at,
        updated_at: session.updated_at,
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

    #[test]
    fn archived_sessions_take_their_title_from_the_first_user_message() {
        let state = serde_json::json!({
            "model": "openai/gpt-5",
            "history": [
                {"role": "assistant", "content": [{"type": "text", "text": "no"}]},
                {"role": "user", "content": [
                    {"type": "image", "mime_type": "image/png", "data": ""},
                    {"type": "text", "text": "\n  Fix the parser  \nplease"},
                ]},
            ],
        });
        let stored = stored_session("s1".into(), 1, 2, &state.to_string()).unwrap();
        assert_eq!(stored.title.as_deref(), Some("Fix the parser"));
        assert_eq!(stored.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!((stored.created_at, stored.updated_at), (1, 2));

        let titled = serde_json::json!({"title": "Mine", "history": [], "pending": [{"id": 1}]});
        let stored = stored_session("s2".into(), 1, 2, &titled.to_string()).unwrap();
        assert_eq!(stored.title.as_deref(), Some("Mine"));
        assert_eq!(stored.model, None);

        for empty in [
            "{}",
            r#"{"history": [], "pending": []}"#,
            r#"{"title": "Empty"}"#,
            "not json",
        ] {
            assert_eq!(stored_session("s3".into(), 1, 2, empty), None, "{empty}");
        }
    }
}
