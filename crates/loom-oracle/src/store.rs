use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

use loom_crypto::{Zeroizing, pairing_id};
use loom_oracle_proto::Role;
use loom_proto::relay::HostId;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::seal::{MasterKey, SealError, Sealer};

mod codespaces;
mod git;
mod models;
mod volumes;

pub use codespaces::{ArchiveRow, CodespaceRow, HostRow, WorkspaceRow};
pub use git::{CredentialCheck, CredentialRow, PresetInput, PresetRow, ProviderRow};
pub use models::{ModelAccountRow, NewModelAccount, SecretRow, SessionRow};
pub use volumes::VolumeRow;

const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE meta (
    key TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE peers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    role TEXT NOT NULL,
    label TEXT NOT NULL,
    pubkey BLOB NOT NULL UNIQUE,
    push_token TEXT,
    pairing TEXT,
    added_at INTEGER NOT NULL,
    last_seen INTEGER
);
CREATE UNIQUE INDEX peers_role_label ON peers (role, label);
CREATE TABLE pairings (
    id TEXT PRIMARY KEY,
    role TEXT NOT NULL,
    secret BLOB NOT NULL,
    created_by INTEGER,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE relay_hosts (
    host_id TEXT PRIMARY KEY,
    label TEXT NOT NULL,
    added_at INTEGER NOT NULL
);
CREATE TABLE audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    peer INTEGER,
    action TEXT NOT NULL,
    detail TEXT NOT NULL
);
",
    "
ALTER TABLE pairings ADD COLUMN target TEXT;
CREATE TABLE hosts (
    peer INTEGER PRIMARY KEY REFERENCES peers (id) ON DELETE CASCADE,
    report TEXT,
    reported_at INTEGER,
    environment TEXT NOT NULL DEFAULT '{}'
);
INSERT INTO hosts (peer) SELECT id FROM peers WHERE role = 'host';
CREATE TABLE workspaces (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    host INTEGER NOT NULL REFERENCES hosts (peer),
    created_at INTEGER NOT NULL
);
CREATE TABLE workspace_repositories (
    workspace INTEGER NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    url TEXT NOT NULL,
    directory TEXT NOT NULL,
    base_branch TEXT,
    PRIMARY KEY (workspace, position)
);
CREATE TABLE codespaces (
    id TEXT PRIMARY KEY,
    workspace INTEGER NOT NULL REFERENCES workspaces (id),
    host INTEGER NOT NULL REFERENCES hosts (peer),
    peer INTEGER REFERENCES peers (id) ON DELETE SET NULL,
    state TEXT NOT NULL,
    cpus INTEGER NOT NULL,
    memory_mib INTEGER NOT NULL,
    instance TEXT NOT NULL,
    branch TEXT NOT NULL,
    base_branch TEXT,
    client_version TEXT,
    agent TEXT,
    step TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX codespaces_host ON codespaces (host);
CREATE INDEX codespaces_peer ON codespaces (peer);
",
    "
CREATE TABLE providers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    web_url TEXT NOT NULL UNIQUE,
    api_url TEXT NOT NULL,
    git_name TEXT,
    git_email TEXT,
    route TEXT NOT NULL DEFAULT 'direct',
    created_at INTEGER NOT NULL
);
CREATE TABLE git_credentials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    provider INTEGER NOT NULL REFERENCES providers (id),
    owner TEXT,
    repository TEXT,
    token BLOB NOT NULL,
    login TEXT,
    user_id INTEGER,
    expires_at INTEGER,
    checked_at INTEGER,
    check_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE presets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    yaml TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE preset_git_credentials (
    preset INTEGER NOT NULL REFERENCES presets (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    credential INTEGER NOT NULL REFERENCES git_credentials (id),
    PRIMARY KEY (preset, position)
);
CREATE TABLE volumes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    size_mib INTEGER,
    path TEXT,
    deleting INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE volume_hosts (
    volume INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    host INTEGER NOT NULL REFERENCES peers (id) ON DELETE CASCADE,
    PRIMARY KEY (volume, host)
);
CREATE TABLE workspace_volumes (
    workspace INTEGER NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    position INTEGER NOT NULL,
    volume INTEGER NOT NULL REFERENCES volumes (id),
    path TEXT NOT NULL,
    read_only INTEGER NOT NULL,
    PRIMARY KEY (workspace, position)
);
CREATE INDEX workspace_volumes_volume ON workspace_volumes (volume);
CREATE TABLE codespace_volumes (
    codespace TEXT NOT NULL REFERENCES codespaces (id) ON DELETE CASCADE,
    volume INTEGER NOT NULL REFERENCES volumes (id) ON DELETE CASCADE,
    PRIMARY KEY (codespace, volume)
);
CREATE INDEX codespace_volumes_volume ON codespace_volumes (volume);
ALTER TABLE workspace_repositories ADD COLUMN provider INTEGER REFERENCES providers (id);
ALTER TABLE codespaces ADD COLUMN preset INTEGER REFERENCES presets (id) ON DELETE SET NULL;
ALTER TABLE codespaces ADD COLUMN repositories TEXT NOT NULL DEFAULT '[]';
",
    "
CREATE TABLE model_accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    credentials BLOB NOT NULL,
    email TEXT,
    organization TEXT,
    account_id TEXT,
    error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE secrets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    value BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
ALTER TABLE presets ADD COLUMN agents_md TEXT NOT NULL DEFAULT '';
CREATE TABLE preset_files (
    preset INTEGER NOT NULL REFERENCES presets (id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    content TEXT NOT NULL,
    PRIMARY KEY (preset, path)
);
CREATE TABLE preset_model_accounts (
    preset INTEGER NOT NULL REFERENCES presets (id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    account INTEGER NOT NULL REFERENCES model_accounts (id),
    PRIMARY KEY (preset, provider)
);
CREATE INDEX preset_model_accounts_account ON preset_model_accounts (account);
CREATE TABLE preset_secrets (
    preset INTEGER NOT NULL REFERENCES presets (id) ON DELETE CASCADE,
    variable TEXT NOT NULL,
    secret INTEGER NOT NULL REFERENCES secrets (id),
    PRIMARY KEY (preset, variable)
);
CREATE INDEX preset_secrets_secret ON preset_secrets (secret);
CREATE TABLE notes (
    workspace INTEGER PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    text TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE sessions (
    codespace TEXT NOT NULL REFERENCES codespaces (id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    title TEXT,
    state TEXT NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0,
    model TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (codespace, id)
);
ALTER TABLE codespaces ADD COLUMN first_message TEXT;
",
    "
ALTER TABLE hosts ADD COLUMN ssh TEXT;
ALTER TABLE hosts ADD COLUMN checks TEXT;
ALTER TABLE hosts ADD COLUMN checked_at INTEGER;
",
    "
ALTER TABLE hosts ADD COLUMN limits TEXT NOT NULL DEFAULT '{}';
ALTER TABLE codespaces ADD COLUMN stopped_at INTEGER;
ALTER TABLE codespaces ADD COLUMN active INTEGER;
ALTER TABLE codespaces ADD COLUMN active_at INTEGER;
ALTER TABLE codespaces ADD COLUMN metrics TEXT;
CREATE INDEX codespaces_state ON codespaces (state);
",
    "
ALTER TABLE codespaces ADD COLUMN archive_size INTEGER;
ALTER TABLE codespaces ADD COLUMN archive_at INTEGER;
ALTER TABLE codespaces ADD COLUMN archive_error TEXT;
",
];

const KEY_CHECK: &str = "master_key_check";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Seal(#[from] SealError),
    #[error("cannot open the store {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("the store has version {found}; this oracle knows versions up to {known}")]
    TooNew { found: usize, known: usize },
    #[error("the master key does not match the store")]
    WrongKey,
    #[error("the store is locked: it has no master key")]
    Locked,
    #[error("{0}")]
    Conflict(String),
    #[error("the store has a malformed {0}")]
    Malformed(&'static str),
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRow {
    pub id: u64,
    pub role: Role,
    pub label: String,
    pub pubkey: [u8; 32],
    pub push_token: Option<String>,
    pub pairing: Option<String>,
    pub added_at: u64,
    pub last_seen: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRow {
    pub id: String,
    pub role: Role,
    pub created_by: Option<u64>,
    pub created_at: u64,
    pub expires_at: u64,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayHostRow {
    pub host: HostId,
    pub label: String,
    pub added_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    pub id: u64,
    pub at: u64,
    pub peer: Option<u64>,
    pub peer_label: Option<String>,
    pub action: String,
    pub detail: String,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

const PEER_COLUMNS: &str = "id, role, label, pubkey, push_token, pairing, added_at, last_seen";

fn peer_row(row: &Row<'_>) -> rusqlite::Result<PeerRow> {
    let pubkey: Vec<u8> = row.get(3)?;
    let pubkey = pubkey.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Blob,
            "a peer key is 32 bytes".into(),
        )
    })?;
    Ok(PeerRow {
        id: to_u64(row.get(0)?),
        role: Role::parse(&row.get::<_, String>(1)?),
        label: row.get(2)?,
        pubkey,
        push_token: row.get(4)?,
        pairing: row.get(5)?,
        added_at: to_u64(row.get(6)?),
        last_seen: row.get::<_, Option<i64>>(7)?.map(to_u64),
    })
}

const PAIRING_COLUMNS: &str = "id, role, created_by, created_at, expires_at, target";

fn pairing_row(row: &Row<'_>) -> rusqlite::Result<PairingRow> {
    Ok(PairingRow {
        id: row.get(0)?,
        role: Role::parse(&row.get::<_, String>(1)?),
        created_by: row.get::<_, Option<i64>>(2)?.map(to_u64),
        created_at: to_u64(row.get(3)?),
        expires_at: to_u64(row.get(4)?),
        target: row.get(5)?,
    })
}

fn secret_context(id: &str) -> String {
    format!("pairings.secret/{id}")
}

pub struct Store {
    conn: Mutex<Connection>,
    sealer: OnceLock<Sealer>,
}

impl Store {
    pub fn open(path: &Path) -> StoreResult<Self> {
        let io = |source| StoreError::Io {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(io)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
        }
        let conn = Connection::open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::setup(conn)
    }

    pub fn open_in_memory() -> StoreResult<Self> {
        Self::setup(Connection::open_in_memory()?)
    }

    fn setup(mut conn: Connection) -> StoreResult<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            sealer: OnceLock::new(),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sealer(&self) -> StoreResult<&Sealer> {
        self.sealer.get().ok_or(StoreError::Locked)
    }

    pub fn has_key(&self) -> StoreResult<bool> {
        Ok(self.meta(KEY_CHECK)?.is_some())
    }

    pub fn unlock(&self, key: &MasterKey) -> StoreResult<()> {
        let check = key.check();
        match self.meta(KEY_CHECK)? {
            Some(stored) if stored == check => {}
            Some(_) => return Err(StoreError::WrongKey),
            None => self.set_meta(KEY_CHECK, &check)?,
        }
        let _ = self.sealer.set(key.sealer());
        Ok(())
    }

    pub fn meta(&self, key: &str) -> StoreResult<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &[u8]) -> StoreResult<()> {
        self.conn().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn peers(&self, role: Option<Role>) -> StoreResult<Vec<PeerRow>> {
        let conn = self.conn();
        let rows = match role {
            Some(role) => conn
                .prepare(&format!(
                    "SELECT {PEER_COLUMNS} FROM peers WHERE role = ?1 ORDER BY id"
                ))?
                .query_map([role.as_str()], peer_row)?
                .collect::<Result<Vec<_>, _>>()?,
            None => conn
                .prepare(&format!("SELECT {PEER_COLUMNS} FROM peers ORDER BY id"))?
                .query_map([], peer_row)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    pub fn peer(&self, id: u64) -> StoreResult<Option<PeerRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {PEER_COLUMNS} FROM peers WHERE id = ?1"),
                [to_i64(id)],
                peer_row,
            )
            .optional()?)
    }

    pub fn peer_by_key(&self, pubkey: &[u8; 32]) -> StoreResult<Option<PeerRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {PEER_COLUMNS} FROM peers WHERE pubkey = ?1"),
                [pubkey.as_slice()],
                peer_row,
            )
            .optional()?)
    }

    pub fn peer_by_pairing(&self, pairing: &str) -> StoreResult<Option<PeerRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {PEER_COLUMNS} FROM peers WHERE pairing = ?1"),
                [pairing],
                peer_row,
            )
            .optional()?)
    }

    pub fn add_peer(
        &self,
        role: Role,
        label: &str,
        pubkey: &[u8; 32],
        pairing: Option<&str>,
    ) -> StoreResult<(PeerRow, Vec<PeerRow>)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if role == Role::Host {
            return codespaces::add_host_peer(tx, label, pubkey, pairing);
        }
        let replaced = tx
            .prepare(&format!(
                "SELECT {PEER_COLUMNS} FROM peers WHERE pubkey = ?1 OR (role = ?2 AND label = ?3)"
            ))?
            .query_map(params![pubkey.as_slice(), role.as_str(), label], peer_row)?
            .collect::<Result<Vec<_>, _>>()?;
        for peer in &replaced {
            tx.execute("DELETE FROM peers WHERE id = ?1", [to_i64(peer.id)])?;
        }
        let now = now_unix();
        tx.execute(
            "INSERT INTO peers (role, label, pubkey, pairing, added_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![role.as_str(), label, pubkey.as_slice(), pairing, to_i64(now)],
        )?;
        let id = to_u64(tx.last_insert_rowid());
        tx.commit()?;
        Ok((
            PeerRow {
                id,
                role,
                label: label.to_owned(),
                pubkey: *pubkey,
                push_token: None,
                pairing: pairing.map(str::to_owned),
                added_at: now,
                last_seen: None,
            },
            replaced,
        ))
    }

    pub fn rename_peer(&self, id: u64, label: &str) -> StoreResult<Option<PeerRow>> {
        let conn = self.conn();
        let Some(peer) = conn
            .query_row(
                &format!("SELECT {PEER_COLUMNS} FROM peers WHERE id = ?1"),
                [to_i64(id)],
                peer_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let taken: Option<i64> = conn
            .query_row(
                "SELECT id FROM peers WHERE role = ?1 AND label = ?2 AND id != ?3",
                params![peer.role.as_str(), label, to_i64(id)],
                |row| row.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Err(StoreError::Conflict(format!(
                "another {} has the label {label}",
                peer.role
            )));
        }
        conn.execute(
            "UPDATE peers SET label = ?1 WHERE id = ?2",
            params![label, to_i64(id)],
        )?;
        Ok(Some(PeerRow {
            label: label.to_owned(),
            ..peer
        }))
    }

    pub fn remove_peer(&self, id: u64) -> StoreResult<Option<PeerRow>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                &format!("DELETE FROM peers WHERE id = ?1 RETURNING {PEER_COLUMNS}"),
                [to_i64(id)],
                peer_row,
            )
            .optional()?)
    }

    pub fn set_push_token(&self, id: u64, token: Option<&str>) -> StoreResult<Option<Vec<u64>>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let exists: Option<i64> = tx
            .query_row("SELECT id FROM peers WHERE id = ?1", [to_i64(id)], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }
        let mut others = Vec::new();
        if let Some(token) = token {
            others = tx
                .prepare("SELECT id FROM peers WHERE push_token = ?1 AND id != ?2")?
                .query_map(params![token, to_i64(id)], |row| row.get::<_, i64>(0))?
                .map(|id| id.map(to_u64))
                .collect::<Result<Vec<_>, _>>()?;
            tx.execute(
                "UPDATE peers SET push_token = NULL WHERE push_token = ?1 AND id != ?2",
                params![token, to_i64(id)],
            )?;
        }
        tx.execute(
            "UPDATE peers SET push_token = ?1 WHERE id = ?2",
            params![token, to_i64(id)],
        )?;
        tx.commit()?;
        Ok(Some(others))
    }

    pub fn touch_peer(&self, id: u64) -> StoreResult<()> {
        self.conn().execute(
            "UPDATE peers SET last_seen = ?1 WHERE id = ?2",
            params![to_i64(now_unix()), to_i64(id)],
        )?;
        Ok(())
    }

    pub fn create_pairing(
        &self,
        role: Role,
        ttl: Duration,
        created_by: Option<u64>,
    ) -> StoreResult<(PairingRow, Zeroizing<[u8; 32]>)> {
        self.create_pairing_for(role, ttl, created_by, None)
    }

    pub fn create_pairing_for(
        &self,
        role: Role,
        ttl: Duration,
        created_by: Option<u64>,
        target: Option<&str>,
    ) -> StoreResult<(PairingRow, Zeroizing<[u8; 32]>)> {
        let sealer = self.sealer()?;
        let mut secret = Zeroizing::new([0u8; 32]);
        rand::fill(&mut *secret);
        let id = pairing_id(&secret);
        let sealed = sealer.seal(&secret_context(&id), &*secret)?;
        let now = now_unix();
        let row = PairingRow {
            id,
            role,
            created_by,
            created_at: now,
            expires_at: now + ttl.as_secs().max(1),
            target: target.map(str::to_owned),
        };
        let conn = self.conn();
        conn.execute("DELETE FROM pairings WHERE expires_at <= ?1", [to_i64(now)])?;
        conn.execute(
            "INSERT INTO pairings (id, role, secret, created_by, created_at, expires_at, target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.id,
                role.as_str(),
                sealed,
                row.created_by.map(to_i64),
                to_i64(row.created_at),
                to_i64(row.expires_at),
                row.target
            ],
        )?;
        Ok((row, secret))
    }

    pub fn take_pairing(&self, id: &str) -> StoreResult<Option<(PairingRow, Zeroizing<[u8; 32]>)>> {
        let sealer = self.sealer()?;
        let taken = self
            .conn()
            .query_row(
                &format!("DELETE FROM pairings WHERE id = ?1 RETURNING {PAIRING_COLUMNS}, secret"),
                [id],
                |row| Ok((pairing_row(row)?, row.get::<_, Vec<u8>>(6)?)),
            )
            .optional()?;
        let Some((row, sealed)) = taken else {
            return Ok(None);
        };
        if row.expires_at <= now_unix() {
            return Ok(None);
        }
        let plain = sealer.open(&secret_context(&row.id), &sealed)?;
        let secret: [u8; 32] = plain
            .as_slice()
            .try_into()
            .map_err(|_| StoreError::Malformed("pairing secret"))?;
        Ok(Some((row, Zeroizing::new(secret))))
    }

    pub fn pairings(&self) -> StoreResult<Vec<PairingRow>> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM pairings WHERE expires_at <= ?1",
            [to_i64(now_unix())],
        )?;
        let rows = conn
            .prepare(&format!(
                "SELECT {PAIRING_COLUMNS} FROM pairings ORDER BY created_at, id"
            ))?
            .query_map([], pairing_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn is_pending(&self, id: &str) -> StoreResult<bool> {
        let found: Option<i64> = self
            .conn()
            .query_row(
                "SELECT 1 FROM pairings WHERE id = ?1 AND expires_at > ?2",
                params![id, to_i64(now_unix())],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn cancel_pairing(&self, id: &str) -> StoreResult<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM pairings WHERE id = ?1", [id])?
            > 0)
    }

    pub fn relay_hosts(&self) -> StoreResult<Vec<RelayHostRow>> {
        let conn = self.conn();
        let rows = conn
            .prepare("SELECT host_id, label, added_at FROM relay_hosts ORDER BY added_at, host_id")?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(host, label, added_at)| {
                Ok(RelayHostRow {
                    host: host
                        .parse()
                        .map_err(|_| StoreError::Malformed("relay host ID"))?,
                    label,
                    added_at: to_u64(added_at),
                })
            })
            .collect()
    }

    pub fn put_relay_host(&self, host: &HostId, label: &str) -> StoreResult<RelayHostRow> {
        let added_at: i64 = self.conn().query_row(
            "INSERT INTO relay_hosts (host_id, label, added_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (host_id) DO UPDATE SET label = excluded.label
             RETURNING added_at",
            params![host.to_string(), label, to_i64(now_unix())],
            |row| row.get(0),
        )?;
        Ok(RelayHostRow {
            host: *host,
            label: label.to_owned(),
            added_at: to_u64(added_at),
        })
    }

    pub fn remove_relay_host(&self, host: &HostId) -> StoreResult<bool> {
        Ok(self.conn().execute(
            "DELETE FROM relay_hosts WHERE host_id = ?1",
            [host.to_string()],
        )? > 0)
    }

    pub fn audit(&self, peer: Option<u64>, action: &str, detail: &str) -> StoreResult<()> {
        self.conn().execute(
            "INSERT INTO audit (at, peer, action, detail) VALUES (?1, ?2, ?3, ?4)",
            params![to_i64(now_unix()), peer.map(to_i64), action, detail],
        )?;
        Ok(())
    }

    pub fn audit_log(&self, limit: usize) -> StoreResult<Vec<AuditRow>> {
        self.audit_page(limit, None)
    }

    pub fn audit_page(&self, limit: usize, before: Option<u64>) -> StoreResult<Vec<AuditRow>> {
        let conn = self.conn();
        let rows = conn
            .prepare(
                "SELECT a.id, a.at, a.peer, p.label, a.action, a.detail
                 FROM audit a LEFT JOIN peers p ON p.id = a.peer
                 WHERE ?2 IS NULL OR a.id < ?2
                 ORDER BY a.id DESC LIMIT ?1",
            )?
            .query_map(params![to_i64(limit as u64), before.map(to_i64)], |row| {
                Ok(AuditRow {
                    id: to_u64(row.get(0)?),
                    at: to_u64(row.get(1)?),
                    peer: row.get::<_, Option<i64>>(2)?.map(to_u64),
                    peer_label: row.get(3)?,
                    action: row.get(4)?,
                    detail: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

fn migrate(conn: &mut Connection) -> StoreResult<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let version = usize::try_from(version).unwrap_or_default();
    if version > MIGRATIONS.len() {
        return Err(StoreError::TooNew {
            found: version,
            known: MIGRATIONS.len(),
        });
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = conn.transaction()?;
        tx.execute_batch(migration)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.unlock(&MasterKey::generate()).unwrap();
        store
    }

    #[test]
    fn migrations_run_once_and_refuse_newer_stores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data/oracle.db");
        let store = Store::open(&path).unwrap();
        store.put_relay_host(&HostId([1; 32]), "a").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.relay_hosts().unwrap().len(), 1);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        store
            .conn()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        drop(store);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::TooNew { found: 99, .. })
        ));
    }

    #[test]
    fn the_master_key_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oracle.db");
        let key = MasterKey::generate();
        let store = Store::open(&path).unwrap();
        assert!(!store.has_key().unwrap());
        assert!(matches!(
            store.create_pairing(Role::Device, Duration::from_secs(60), None),
            Err(StoreError::Locked)
        ));
        store.unlock(&key).unwrap();
        assert!(store.has_key().unwrap());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(matches!(
            store.unlock(&MasterKey::generate()),
            Err(StoreError::WrongKey)
        ));
        store.unlock(&key).unwrap();
    }

    #[test]
    fn pairings_keep_their_secret_sealed_and_work_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oracle.db");
        let store = Store::open(&path).unwrap();
        store.unlock(&MasterKey::generate()).unwrap();
        let (row, secret) = store
            .create_pairing(Role::Device, Duration::from_secs(60), Some(4))
            .unwrap();
        assert_eq!(row.id, pairing_id(&secret));
        assert_eq!(row.created_by, Some(4));
        assert!(store.is_pending(&row.id).unwrap());
        assert_eq!(store.pairings().unwrap(), vec![row.clone()]);
        let sealed: Vec<u8> = store
            .conn()
            .query_row("SELECT secret FROM pairings", [], |r| r.get(0))
            .unwrap();
        assert!(!sealed.windows(32).any(|w| w == secret.as_slice()));

        let (taken, again) = store.take_pairing(&row.id).unwrap().unwrap();
        assert_eq!(taken, row);
        assert_eq!(*again, *secret);
        assert!(store.take_pairing(&row.id).unwrap().is_none());
        assert!(!store.is_pending(&row.id).unwrap());

        let (row, _) = store
            .create_pairing(Role::Host, Duration::from_secs(60), None)
            .unwrap();
        assert!(store.cancel_pairing(&row.id).unwrap());
        assert!(!store.cancel_pairing(&row.id).unwrap());

        let (row, _) = store
            .create_pairing(Role::Device, Duration::from_secs(60), None)
            .unwrap();
        store
            .conn()
            .execute("UPDATE pairings SET expires_at = 1", [])
            .unwrap();
        assert!(!store.is_pending(&row.id).unwrap());
        assert!(store.take_pairing(&row.id).unwrap().is_none());
        assert!(store.pairings().unwrap().is_empty());
    }

    #[test]
    fn peers_replace_their_label_and_key() {
        let store = store();
        let (laptop, replaced) = store
            .add_peer(Role::Device, "laptop", &[1; 32], Some("p1"))
            .unwrap();
        assert!(replaced.is_empty());
        let (phone, _) = store
            .add_peer(Role::Device, "phone", &[2; 32], None)
            .unwrap();
        let (host, _) = store
            .add_peer(Role::Host, "laptop", &[3; 32], None)
            .unwrap();
        assert_eq!(store.peers(Some(Role::Device)).unwrap().len(), 2);
        assert_eq!(store.peers(None).unwrap().len(), 3);
        assert_eq!(store.peer_by_key(&[3; 32]).unwrap().unwrap(), host);
        assert_eq!(store.peer_by_pairing("p1").unwrap().unwrap(), laptop);

        let (again, replaced) = store
            .add_peer(Role::Device, "laptop", &[4; 32], None)
            .unwrap();
        assert_eq!(replaced, vec![laptop.clone()]);
        assert!(again.id > phone.id);
        let (_, replaced) = store
            .add_peer(Role::Device, "tablet", &[2; 32], None)
            .unwrap();
        assert_eq!(replaced, vec![phone]);
        assert!(store.peer(laptop.id).unwrap().is_none());

        assert!(matches!(
            store.rename_peer(again.id, "tablet"),
            Err(StoreError::Conflict(_))
        ));
        let renamed = store.rename_peer(again.id, "desk").unwrap().unwrap();
        assert_eq!(renamed.label, "desk");
        assert_eq!(store.rename_peer(again.id, "desk").unwrap(), Some(renamed));
        assert!(store.rename_peer(999, "x").unwrap().is_none());

        store.touch_peer(again.id).unwrap();
        assert!(store.peer(again.id).unwrap().unwrap().last_seen.is_some());
        assert_eq!(store.remove_peer(again.id).unwrap().unwrap().label, "desk");
        assert!(store.remove_peer(again.id).unwrap().is_none());
    }

    #[test]
    fn push_tokens_belong_to_one_peer() {
        let store = store();
        let (a, _) = store.add_peer(Role::Device, "a", &[1; 32], None).unwrap();
        let (b, _) = store.add_peer(Role::Device, "b", &[2; 32], None).unwrap();
        assert_eq!(store.set_push_token(a.id, Some("t")).unwrap(), Some(vec![]));
        assert_eq!(
            store.set_push_token(b.id, Some("t")).unwrap(),
            Some(vec![a.id])
        );
        assert_eq!(store.peer(a.id).unwrap().unwrap().push_token, None);
        assert_eq!(
            store.peer(b.id).unwrap().unwrap().push_token.as_deref(),
            Some("t")
        );
        assert_eq!(store.set_push_token(b.id, None).unwrap(), Some(vec![]));
        assert_eq!(store.peer(b.id).unwrap().unwrap().push_token, None);
        assert_eq!(store.set_push_token(99, Some("t")).unwrap(), None);
    }

    #[test]
    fn relay_hosts_and_audit() {
        let store = store();
        let host = HostId([7; 32]);
        let first = store.put_relay_host(&host, "devbox").unwrap();
        let renamed = store.put_relay_host(&host, "build").unwrap();
        assert_eq!(renamed.added_at, first.added_at);
        assert_eq!(store.relay_hosts().unwrap(), vec![renamed]);
        assert!(store.remove_relay_host(&host).unwrap());
        assert!(!store.remove_relay_host(&host).unwrap());

        store.audit(None, "first", "a").unwrap();
        store.audit(Some(3), "second", "b").unwrap();
        let log = store.audit_log(10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].action, "second");
        assert_eq!(log[0].peer, Some(3));
        assert_eq!(log[1].peer, None);
    }
}
