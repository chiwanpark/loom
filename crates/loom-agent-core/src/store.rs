use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use loom_llm::Message;
use loom_proto::{SequencedEvent, SessionId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::policy::SessionPolicy;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    state TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    event BLOB NOT NULL,
    PRIMARY KEY (session_id, seq)
);
CREATE TABLE IF NOT EXISTS threads (
    session_id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
";
/// Tables are only ever added, so files stay at version 1 and older agents,
/// which refuse other versions, can still open them.
const SCHEMA_VERSION: &str = "1";
const BATCH_WINDOW: Duration = Duration::from_millis(20);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("event store {path}: {message}")]
    Open { path: PathBuf, message: String },
    #[error("event store: {0}")]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub model: Option<String>,
    pub reasoning: Option<loom_llm::ReasoningEffort>,
    pub policy: SessionPolicy,
    pub history: Vec<Message>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub title: Option<String>,
    /// The model and effort of the last request, so a restart does not
    /// forget what a change is measured against.
    #[serde(default)]
    pub last_used: Option<UsedModel>,
}

/// The model and reasoning effort a request was sent with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsedModel {
    pub model: String,
    pub reasoning: Option<loom_llm::ReasoningEffort>,
}

#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: SessionId,
    pub created_at: u64,
    pub state: SessionState,
    pub events: Vec<SequencedEvent>,
    /// The session's operation thread; empty when it was never written.
    pub thread: String,
}

enum Op {
    Event {
        session: SessionId,
        seq: u64,
        data: Vec<u8>,
    },
    Session {
        session: SessionId,
        created_at: u64,
        state: String,
    },
    Thread {
        session: SessionId,
        text: String,
    },
    Delete(SessionId),
    /// Drops a session's events before `seq`.
    Truncate {
        session: SessionId,
        seq: u64,
    },
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct EventStore {
    tx: std_mpsc::Sender<Op>,
    path: PathBuf,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub fn store_path(data_dir: &Path, workspace: &Path) -> PathBuf {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let key: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    data_dir.join("sessions").join(format!("{key}.sqlite"))
}

fn open(path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| StoreError::Open {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

impl EventStore {
    pub fn open(path: &Path, workspace: &Path) -> Result<(Self, Vec<StoredSession>), StoreError> {
        let conn = open(path)?;
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(version) = version
            && version != SCHEMA_VERSION
        {
            return Err(StoreError::Open {
                path: path.to_path_buf(),
                message: format!("unsupported schema version {version}"),
            });
        }
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema', ?1), ('workspace', ?2)",
            params![SCHEMA_VERSION, workspace.to_string_lossy()],
        )?;
        let sessions = load(&conn)?;
        let (tx, rx) = std_mpsc::channel();
        let thread_path = path.to_path_buf();
        std::thread::Builder::new()
            .name("loom-event-store".into())
            .spawn(move || writer(conn, rx, thread_path))
            .map_err(|e| StoreError::Open {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        Ok((
            Self {
                tx,
                path: path.to_path_buf(),
            },
            sessions,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn delete_session(&self, session: &SessionId) {
        let _ = self.tx.send(Op::Delete(session.clone()));
    }

    /// Drops the session's events numbered below `seq`.
    pub fn truncate(&self, session: &SessionId, seq: u64) {
        let _ = self.tx.send(Op::Truncate {
            session: session.clone(),
            seq,
        });
    }

    pub fn append(&self, session: &SessionId, event: &SequencedEvent) {
        match postcard::to_allocvec(event) {
            Ok(data) => {
                let _ = self.tx.send(Op::Event {
                    session: session.clone(),
                    seq: event.seq,
                    data,
                });
            }
            Err(err) => tracing::error!(error = %err, "could not encode an event for the store"),
        }
    }

    pub fn save_session(&self, session: &SessionId, created_at: u64, state: &SessionState) {
        match serde_json::to_string(state) {
            Ok(state) => {
                let _ = self.tx.send(Op::Session {
                    session: session.clone(),
                    created_at,
                    state,
                });
            }
            Err(err) => tracing::error!(error = %err, "could not encode a session for the store"),
        }
    }

    /// Replaces the session's operation thread.
    pub fn save_thread(&self, session: &SessionId, text: &str) {
        let _ = self.tx.send(Op::Thread {
            session: session.clone(),
            text: text.to_owned(),
        });
    }

    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Op::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

fn load(conn: &Connection) -> Result<Vec<StoredSession>, StoreError> {
    let mut sessions = Vec::new();
    let mut stmt =
        conn.prepare("SELECT id, created_at, state FROM sessions ORDER BY created_at, id")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut events_stmt =
        conn.prepare("SELECT seq, event FROM events WHERE session_id = ?1 ORDER BY seq")?;
    let mut thread_stmt = conn.prepare("SELECT text FROM threads WHERE session_id = ?1")?;
    for row in rows {
        let (id, created_at, state) = row?;
        let state: SessionState = match serde_json::from_str(&state) {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(session = %id, error = %err, "skipping a session that cannot be read");
                continue;
            }
        };
        let mut events = Vec::new();
        let rows = events_stmt.query_map(params![id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for event in rows {
            let (seq, data) = event?;
            match postcard::from_bytes::<SequencedEvent>(&data) {
                Ok(event) => events.push(event),
                Err(err) => {
                    tracing::warn!(session = %id, seq, error = %err, "skipping an event that cannot be read")
                }
            }
        }
        let thread = thread_stmt
            .query_row(params![id], |row| row.get::<_, String>(0))
            .optional()?
            .unwrap_or_default();
        sessions.push(StoredSession {
            id: SessionId(id),
            created_at: created_at as u64,
            state,
            events,
            thread,
        });
    }
    Ok(sessions)
}

fn apply(
    conn: &Connection,
    op: Op,
    flushes: &mut Vec<oneshot::Sender<()>>,
    deleted: &mut HashSet<SessionId>,
) -> rusqlite::Result<()> {
    match op {
        Op::Event { session, .. }
        | Op::Session { session, .. }
        | Op::Thread { session, .. }
        | Op::Truncate { session, .. }
            if deleted.contains(&session) => {}
        Op::Event { session, seq, data } => {
            conn.execute(
                "INSERT OR REPLACE INTO events (session_id, seq, event) VALUES (?1, ?2, ?3)",
                params![session.0, seq as i64, data],
            )?;
        }
        Op::Session {
            session,
            created_at,
            state,
        } => {
            conn.execute(
                "INSERT INTO sessions (id, created_at, updated_at, state) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at, state = excluded.state",
                params![session.0, created_at as i64, now_unix() as i64, state],
            )?;
        }
        Op::Thread { session, text } => {
            conn.execute(
                "INSERT INTO threads (session_id, text, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
                params![session.0, text, now_unix() as i64],
            )?;
        }
        Op::Delete(session) => {
            conn.execute(
                "DELETE FROM events WHERE session_id = ?1",
                params![session.0],
            )?;
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![session.0])?;
            conn.execute(
                "DELETE FROM threads WHERE session_id = ?1",
                params![session.0],
            )?;
            deleted.insert(session);
        }
        Op::Truncate { session, seq } => {
            conn.execute(
                "DELETE FROM events WHERE session_id = ?1 AND seq < ?2",
                params![session.0, seq as i64],
            )?;
        }
        Op::Flush(done) => flushes.push(done),
    }
    Ok(())
}

fn writer(mut conn: Connection, rx: std_mpsc::Receiver<Op>, path: PathBuf) {
    let mut deleted = HashSet::new();
    while let Ok(first) = rx.recv() {
        let mut flushes = Vec::new();
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            apply(&tx, first, &mut flushes, &mut deleted)?;
            let deadline = std::time::Instant::now() + BATCH_WINDOW;
            while let Ok(op) =
                rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                apply(&tx, op, &mut flushes, &mut deleted)?;
            }
            tx.commit()
        })();
        if let Err(err) = result {
            tracing::error!(path = %path.display(), error = %err, "could not write to the event store");
        }
        for done in flushes {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> SessionId {
        SessionId(name.into())
    }

    fn count(path: &Path, sql: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(sql, [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn threads_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        let (store, _) = EventStore::open(&path, dir.path()).unwrap();
        for name in ["a", "b"] {
            store.save_session(&id(name), 1, &SessionState::default());
        }
        store.save_thread(&id("a"), "first");
        store.save_thread(&id("a"), "- plan\n- done");
        store.flush().await;
        drop(store);

        let (_, sessions) = EventStore::open(&path, dir.path()).unwrap();
        let threads: Vec<_> = sessions
            .iter()
            .map(|s| (s.id.0.as_str(), s.thread.as_str()))
            .collect();
        assert_eq!(threads, [("a", "- plan\n- done"), ("b", "")]);
    }

    #[tokio::test]
    async fn deleting_a_session_removes_its_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        let (store, _) = EventStore::open(&path, dir.path()).unwrap();
        for name in ["a", "b"] {
            store.save_session(&id(name), 1, &SessionState::default());
            store.save_thread(&id(name), name);
        }
        store.flush().await;
        store.delete_session(&id("a"));
        // A write that was already on its way when the session went.
        store.save_thread(&id("a"), "late");
        store.flush().await;
        assert_eq!(
            count(&path, "SELECT count(*) FROM threads WHERE session_id = 'a'"),
            0
        );
        assert_eq!(count(&path, "SELECT count(*) FROM threads"), 1);
    }

    #[tokio::test]
    async fn opens_a_file_written_before_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        let state = serde_json::to_string(&SessionState::default()).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch(&format!(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE sessions (id TEXT PRIMARY KEY, created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL, state TEXT NOT NULL);
                 CREATE TABLE events (session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                     event BLOB NOT NULL, PRIMARY KEY (session_id, seq));
                 INSERT INTO meta VALUES ('schema', '1');
                 INSERT INTO sessions VALUES ('old', 1, 1, '{state}');"
            ))
            .unwrap();

        let (store, sessions) = EventStore::open(&path, dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].thread, "");
        store.save_thread(&id("old"), "notes");
        store.flush().await;
        drop(store);

        let (_, sessions) = EventStore::open(&path, dir.path()).unwrap();
        assert_eq!(sessions[0].thread, "notes");
        // Older agents refuse any other version.
        assert_eq!(
            count(
                &path,
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema'"
            ),
            1
        );
    }
}
