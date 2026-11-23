use loom_crypto::Zeroizing;
use loom_oracle_proto::{AccountKind, SessionSummary};
use rusqlite::{OptionalExtension, Row, params};

use super::{Store, StoreError, StoreResult, now_unix, to_i64, to_u64};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAccountRow {
    pub id: u64,
    pub name: String,
    pub provider: String,
    pub kind: AccountKind,
    pub email: Option<String>,
    pub organization: Option<String>,
    pub account_id: Option<String>,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewModelAccount<'a> {
    pub name: &'a str,
    pub provider: &'a str,
    pub kind: AccountKind,
    pub credentials: &'a [u8],
    pub email: Option<&'a str>,
    pub organization: Option<&'a str>,
    pub account_id: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRow {
    pub id: u64,
    pub name: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub codespace: String,
    pub workspace: String,
    pub session: SessionSummary,
}

const ACCOUNT_SELECT: &str = "SELECT id, name, provider, kind, email, organization, account_id, \
     error, created_at, updated_at FROM model_accounts";

pub(super) fn account_row(row: &Row<'_>) -> rusqlite::Result<ModelAccountRow> {
    Ok(ModelAccountRow {
        id: to_u64(row.get(0)?),
        name: row.get(1)?,
        provider: row.get(2)?,
        kind: AccountKind::parse(&row.get::<_, String>(3)?),
        email: row.get(4)?,
        organization: row.get(5)?,
        account_id: row.get(6)?,
        error: row.get(7)?,
        created_at: to_u64(row.get(8)?),
        updated_at: to_u64(row.get(9)?),
    })
}

const SECRET_SELECT: &str = "SELECT id, name, created_at, updated_at FROM secrets";

pub(super) fn secret_row(row: &Row<'_>) -> rusqlite::Result<SecretRow> {
    Ok(SecretRow {
        id: to_u64(row.get(0)?),
        name: row.get(1)?,
        created_at: to_u64(row.get(2)?),
        updated_at: to_u64(row.get(3)?),
    })
}

fn account_context(id: u64) -> String {
    format!("model_accounts.credentials/{id}")
}

fn secret_context(id: u64) -> String {
    format!("secrets.value/{id}")
}

const SESSION_SELECT: &str = "SELECT s.codespace, w.name, s.id, s.title, s.state, s.archived, \
     s.model, s.created_at, s.updated_at FROM sessions s \
     JOIN codespaces c ON c.id = s.codespace JOIN workspaces w ON w.id = c.workspace";

fn session_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        codespace: row.get(0)?,
        workspace: row.get(1)?,
        session: SessionSummary {
            id: row.get(2)?,
            title: row.get(3)?,
            state: row.get(4)?,
            archived: row.get::<_, i64>(5)? != 0,
            model: row.get(6)?,
            created_at: to_u64(row.get(7)?),
            updated_at: to_u64(row.get(8)?),
        },
    })
}

impl Store {
    pub fn model_accounts(&self) -> StoreResult<Vec<ModelAccountRow>> {
        let rows = self
            .conn()
            .prepare(&format!("{ACCOUNT_SELECT} ORDER BY name"))?
            .query_map([], account_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn model_account_by_name(&self, name: &str) -> StoreResult<Option<ModelAccountRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{ACCOUNT_SELECT} WHERE name = ?1"),
                [name],
                account_row,
            )
            .optional()?)
    }

    pub fn model_account(&self, id: u64) -> StoreResult<Option<ModelAccountRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{ACCOUNT_SELECT} WHERE id = ?1"),
                [to_i64(id)],
                account_row,
            )
            .optional()?)
    }

    pub fn put_model_account(&self, account: &NewModelAccount<'_>) -> StoreResult<ModelAccountRow> {
        let sealer = self.sealer()?;
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let now = to_i64(now_unix());
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, provider FROM model_accounts WHERE name = ?1",
                    [account.name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let id = match existing {
                Some((_, provider)) if provider != account.provider => {
                    return Err(StoreError::Conflict(format!(
                        "the model account {} is for {provider}; use a different name for {}",
                        account.name, account.provider
                    )));
                }
                Some((id, _)) => {
                    tx.execute(
                        "UPDATE model_accounts SET kind = ?1, email = ?2, organization = ?3,
                             account_id = ?4, error = NULL, updated_at = ?5 WHERE id = ?6",
                        params![
                            account.kind.as_str(),
                            account.email,
                            account.organization,
                            account.account_id,
                            now,
                            id
                        ],
                    )?;
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO model_accounts (name, provider, kind, credentials, email,
                             organization, account_id, created_at, updated_at)
                         VALUES (?1, ?2, ?3, x'', ?4, ?5, ?6, ?7, ?7)",
                        params![
                            account.name,
                            account.provider,
                            account.kind.as_str(),
                            account.email,
                            account.organization,
                            account.account_id,
                            now
                        ],
                    )?;
                    tx.last_insert_rowid()
                }
            };
            let sealed = sealer.seal(&account_context(to_u64(id)), account.credentials)?;
            tx.execute(
                "UPDATE model_accounts SET credentials = ?1 WHERE id = ?2",
                params![sealed, id],
            )?;
            tx.commit()?;
        }
        self.model_account_by_name(account.name)?
            .ok_or(StoreError::Malformed("new model account"))
    }

    pub fn model_account_credentials(&self, id: u64) -> StoreResult<Zeroizing<Vec<u8>>> {
        let sealer = self.sealer()?;
        let sealed: Vec<u8> = self.conn().query_row(
            "SELECT credentials FROM model_accounts WHERE id = ?1",
            [to_i64(id)],
            |r| r.get(0),
        )?;
        Ok(sealer.open(&account_context(id), &sealed)?)
    }

    pub fn set_model_account_credentials(&self, id: u64, credentials: &[u8]) -> StoreResult<()> {
        let sealed = self.sealer()?.seal(&account_context(id), credentials)?;
        self.conn().execute(
            "UPDATE model_accounts SET credentials = ?1, error = NULL WHERE id = ?2",
            params![sealed, to_i64(id)],
        )?;
        Ok(())
    }

    pub fn set_model_account_error(&self, id: u64, error: Option<&str>) -> StoreResult<bool> {
        let changed = self.conn().execute(
            "UPDATE model_accounts SET error = ?1 WHERE id = ?2 AND error IS NOT ?1",
            params![error, to_i64(id)],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_model_account(&self, name: &str) -> StoreResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(id) = tx
            .query_row(
                "SELECT id FROM model_accounts WHERE name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        else {
            return Ok(false);
        };
        let presets = tx
            .prepare(
                "SELECT DISTINCT p.name FROM preset_model_accounts a
                 JOIN presets p ON p.id = a.preset WHERE a.account = ?1 ORDER BY p.name",
            )?
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !presets.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the presets {} use this model account",
                presets.join(", ")
            )));
        }
        tx.execute("DELETE FROM model_accounts WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn presets_with_account(&self, account: u64) -> StoreResult<Vec<String>> {
        let names = self
            .conn()
            .prepare(
                "SELECT DISTINCT p.name FROM preset_model_accounts a
                 JOIN presets p ON p.id = a.preset WHERE a.account = ?1 ORDER BY p.name",
            )?
            .query_map([to_i64(account)], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }

    pub fn secrets(&self) -> StoreResult<Vec<SecretRow>> {
        let rows = self
            .conn()
            .prepare(&format!("{SECRET_SELECT} ORDER BY name"))?
            .query_map([], secret_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn secret_by_name(&self, name: &str) -> StoreResult<Option<SecretRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{SECRET_SELECT} WHERE name = ?1"),
                [name],
                secret_row,
            )
            .optional()?)
    }

    pub fn put_secret(&self, name: &str, value: &str) -> StoreResult<SecretRow> {
        let sealer = self.sealer()?;
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let now = to_i64(now_unix());
            let id: i64 = tx.query_row(
                "INSERT INTO secrets (name, value, created_at, updated_at) VALUES (?1, x'', ?2, ?2)
                 ON CONFLICT (name) DO UPDATE SET updated_at = excluded.updated_at
                 RETURNING id",
                params![name, now],
                |r| r.get(0),
            )?;
            let sealed = sealer.seal(&secret_context(to_u64(id)), value.as_bytes())?;
            tx.execute(
                "UPDATE secrets SET value = ?1 WHERE id = ?2",
                params![sealed, id],
            )?;
            tx.commit()?;
        }
        self.secret_by_name(name)?
            .ok_or(StoreError::Malformed("new secret"))
    }

    pub fn secret_value(&self, id: u64) -> StoreResult<Zeroizing<String>> {
        let sealer = self.sealer()?;
        let sealed: Vec<u8> = self.conn().query_row(
            "SELECT value FROM secrets WHERE id = ?1",
            [to_i64(id)],
            |r| r.get(0),
        )?;
        let plain = sealer.open(&secret_context(id), &sealed)?;
        let text = std::str::from_utf8(&plain).map_err(|_| StoreError::Malformed("secret"))?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    pub fn delete_secret(&self, name: &str) -> StoreResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(id) = tx
            .query_row("SELECT id FROM secrets WHERE name = ?1", [name], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        else {
            return Ok(false);
        };
        let presets = tx
            .prepare(
                "SELECT DISTINCT p.name FROM preset_secrets s
                 JOIN presets p ON p.id = s.preset WHERE s.secret = ?1 ORDER BY p.name",
            )?
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !presets.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the presets {} use this secret",
                presets.join(", ")
            )));
        }
        tx.execute("DELETE FROM secrets WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn presets_with_secret(&self, secret: u64) -> StoreResult<Vec<String>> {
        let names = self
            .conn()
            .prepare(
                "SELECT DISTINCT p.name FROM preset_secrets s
                 JOIN presets p ON p.id = s.preset WHERE s.secret = ?1 ORDER BY p.name",
            )?
            .query_map([to_i64(secret)], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }

    pub fn notes(&self, workspace: u64) -> StoreResult<Option<(String, u64)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT text, updated_at FROM notes WHERE workspace = ?1",
                [to_i64(workspace)],
                |r| Ok((r.get(0)?, to_u64(r.get(1)?))),
            )
            .optional()?)
    }

    pub fn set_notes(&self, workspace: u64, text: &str) -> StoreResult<u64> {
        let now = now_unix();
        self.conn().execute(
            "INSERT INTO notes (workspace, text, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (workspace) DO UPDATE SET text = excluded.text,
                 updated_at = excluded.updated_at",
            params![to_i64(workspace), text, to_i64(now)],
        )?;
        Ok(now)
    }

    pub fn sessions(&self, codespace: Option<&str>) -> StoreResult<Vec<SessionRow>> {
        let conn = self.conn();
        let rows = match codespace {
            Some(codespace) => conn
                .prepare(&format!(
                    "{SESSION_SELECT} WHERE s.codespace = ?1 ORDER BY s.updated_at DESC, s.id"
                ))?
                .query_map([codespace], session_row)?
                .collect::<Result<Vec<_>, _>>()?,
            None => conn
                .prepare(&format!(
                    "{SESSION_SELECT} ORDER BY s.updated_at DESC, s.id"
                ))?
                .query_map([], session_row)?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok(rows)
    }

    pub fn put_sessions(
        &self,
        codespace: &str,
        sessions: &[SessionSummary],
        removed: &[String],
        full: bool,
    ) -> StoreResult<Vec<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut gone: Vec<String> = Vec::new();
        if full {
            let known = tx
                .prepare("SELECT id FROM sessions WHERE codespace = ?1")?
                .query_map([codespace], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            gone.extend(
                known
                    .into_iter()
                    .filter(|id| !sessions.iter().any(|s| &s.id == id)),
            );
        }
        gone.extend(removed.iter().cloned());
        gone.sort();
        gone.dedup();
        let mut deleted = Vec::new();
        for id in gone {
            if tx.execute(
                "DELETE FROM sessions WHERE codespace = ?1 AND id = ?2",
                params![codespace, id],
            )? > 0
            {
                deleted.push(id);
            }
        }
        for session in sessions {
            tx.execute(
                "INSERT INTO sessions (codespace, id, title, state, archived, model, created_at,
                     updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (codespace, id) DO UPDATE SET title = excluded.title,
                     state = excluded.state, archived = excluded.archived, model = excluded.model,
                     updated_at = excluded.updated_at",
                params![
                    codespace,
                    session.id,
                    session.title,
                    session.state,
                    i64::from(session.archived),
                    session.model,
                    to_i64(session.created_at),
                    to_i64(session.updated_at),
                ],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn take_first_message(&self, codespace: &str) -> StoreResult<Option<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let message: Option<String> = tx
            .query_row(
                "SELECT first_message FROM codespaces WHERE id = ?1",
                [codespace],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if message.is_some() {
            tx.execute(
                "UPDATE codespaces SET first_message = NULL WHERE id = ?1",
                [codespace],
            )?;
        }
        tx.commit()?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::MasterKey;

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.unlock(&MasterKey::generate()).unwrap();
        store
    }

    fn account<'a>(name: &'a str, provider: &'a str, credentials: &'a [u8]) -> NewModelAccount<'a> {
        NewModelAccount {
            name,
            provider,
            kind: AccountKind::SignIn,
            credentials,
            email: Some("me@example.com"),
            organization: None,
            account_id: Some("acc-1"),
        }
    }

    #[test]
    fn model_accounts_are_sealed_and_keep_their_provider() {
        let store = store();
        let row = store
            .put_model_account(&account("work", "anthropic", b"{\"version\":1}"))
            .unwrap();
        assert_eq!(row.provider, "anthropic");
        assert_eq!(row.email.as_deref(), Some("me@example.com"));
        assert_eq!(
            store.model_account_credentials(row.id).unwrap().as_slice(),
            b"{\"version\":1}"
        );
        let raw: Vec<u8> = store
            .conn()
            .query_row("SELECT credentials FROM model_accounts", [], |r| r.get(0))
            .unwrap();
        assert!(!raw.windows(7).any(|w| w == b"version"));

        let again = store
            .put_model_account(&account("work", "anthropic", b"{}"))
            .unwrap();
        assert_eq!(again.id, row.id);
        assert_eq!(
            store.model_account_credentials(row.id).unwrap().as_slice(),
            b"{}"
        );
        assert!(matches!(
            store.put_model_account(&account("work", "openai-codex", b"{}")),
            Err(StoreError::Conflict(_))
        ));

        assert!(
            store
                .set_model_account_error(row.id, Some("expired"))
                .unwrap()
        );
        assert!(
            !store
                .set_model_account_error(row.id, Some("expired"))
                .unwrap()
        );
        store.set_model_account_credentials(row.id, b"[]").unwrap();
        assert_eq!(store.model_account(row.id).unwrap().unwrap().error, None);
        assert!(store.delete_model_account("work").unwrap());
        assert!(!store.delete_model_account("work").unwrap());
    }

    #[test]
    fn secrets_are_sealed() {
        let store = store();
        let row = store.put_secret("npm", "token-1").unwrap();
        assert_eq!(store.secret_value(row.id).unwrap().as_str(), "token-1");
        let again = store.put_secret("npm", "token-2").unwrap();
        assert_eq!(again.id, row.id);
        assert_eq!(store.secret_value(row.id).unwrap().as_str(), "token-2");
        assert_eq!(store.secrets().unwrap().len(), 1);
        assert!(store.delete_secret("npm").unwrap());
        assert!(store.secret_by_name("npm").unwrap().is_none());
    }
}
