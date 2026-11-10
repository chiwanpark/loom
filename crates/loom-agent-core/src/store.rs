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
";
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
}

#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: SessionId,
    pub created_at: u64,
    pub state: SessionState,
    pub events: Vec<SequencedEvent>,
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
        sessions.push(StoredSession {
            id: SessionId(id),
            created_at: created_at as u64,
            state,
            events,
        });
    }
    Ok(sessions)
}

fn apply(
    conn: &Connection,
    op: Op,
    flushes: &mut Vec<oneshot::Sender<()>>,
) -> rusqlite::Result<()> {
    match op {
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
        Op::Flush(done) => flushes.push(done),
    }
    Ok(())
}

fn writer(mut conn: Connection, rx: std_mpsc::Receiver<Op>, path: PathBuf) {
    while let Ok(first) = rx.recv() {
        let mut flushes = Vec::new();
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            apply(&tx, first, &mut flushes)?;
            let deadline = std::time::Instant::now() + BATCH_WINDOW;
            while let Ok(op) =
                rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                apply(&tx, op, &mut flushes)?;
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
