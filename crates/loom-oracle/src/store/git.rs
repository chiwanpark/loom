use loom_crypto::Zeroizing;
use loom_oracle_proto::{CodespaceState, ProviderRoute};
use rusqlite::{OptionalExtension, Row, Transaction, params};

use super::{Store, StoreError, StoreResult, now_unix, to_i64, to_u64};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: u64,
    pub name: String,
    pub web_url: String,
    pub api_url: String,
    pub git_name: Option<String>,
    pub git_email: Option<String>,
    pub route: ProviderRoute,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialRow {
    pub id: u64,
    pub name: String,
    pub provider: u64,
    pub provider_name: String,
    pub owner: Option<String>,
    pub repository: Option<String>,
    pub login: Option<String>,
    pub user_id: Option<u64>,
    pub expires_at: Option<u64>,
    pub checked_at: Option<u64>,
    pub check_error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialCheck {
    pub login: Option<String>,
    pub user_id: Option<u64>,
    pub expires_at: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetRow {
    pub id: u64,
    pub name: String,
    pub yaml: String,
    pub credentials: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

const PROVIDER_SELECT: &str =
    "SELECT id, name, web_url, api_url, git_name, git_email, route, created_at FROM providers";

fn provider_row(row: &Row<'_>) -> rusqlite::Result<ProviderRow> {
    Ok(ProviderRow {
        id: to_u64(row.get(0)?),
        name: row.get(1)?,
        web_url: row.get(2)?,
        api_url: row.get(3)?,
        git_name: row.get(4)?,
        git_email: row.get(5)?,
        route: ProviderRoute::parse(&row.get::<_, String>(6)?),
        created_at: to_u64(row.get(7)?),
    })
}

const CREDENTIAL_SELECT: &str = "SELECT c.id, c.name, c.provider, p.name, c.owner, c.repository, \
     c.login, c.user_id, c.expires_at, c.checked_at, c.check_error, c.created_at, c.updated_at \
     FROM git_credentials c JOIN providers p ON p.id = c.provider";

fn credential_row(row: &Row<'_>) -> rusqlite::Result<CredentialRow> {
    Ok(CredentialRow {
        id: to_u64(row.get(0)?),
        name: row.get(1)?,
        provider: to_u64(row.get(2)?),
        provider_name: row.get(3)?,
        owner: row.get(4)?,
        repository: row.get(5)?,
        login: row.get(6)?,
        user_id: row.get::<_, Option<i64>>(7)?.map(to_u64),
        expires_at: row.get::<_, Option<i64>>(8)?.map(to_u64),
        checked_at: row.get::<_, Option<i64>>(9)?.map(to_u64),
        check_error: row.get(10)?,
        created_at: to_u64(row.get(11)?),
        updated_at: to_u64(row.get(12)?),
    })
}

fn token_context(name: &str) -> String {
    format!("git_credentials.token/{name}")
}

fn names(tx: &Transaction<'_>, sql: &str, id: u64) -> StoreResult<Vec<String>> {
    let names = tx
        .prepare(sql)?
        .query_map([to_i64(id)], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(names)
}

impl Store {
    pub fn providers(&self) -> StoreResult<Vec<ProviderRow>> {
        let rows = self
            .conn()
            .prepare(&format!("{PROVIDER_SELECT} ORDER BY name"))?
            .query_map([], provider_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn provider_by_name(&self, name: &str) -> StoreResult<Option<ProviderRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{PROVIDER_SELECT} WHERE name = ?1"),
                [name],
                provider_row,
            )
            .optional()?)
    }

    pub fn put_provider(&self, row: &ProviderRow) -> StoreResult<ProviderRow> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let taken: Option<String> = tx
            .query_row(
                "SELECT name FROM providers WHERE web_url = ?1 AND name != ?2",
                params![row.web_url, row.name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = taken {
            return Err(StoreError::Conflict(format!(
                "the provider {other} has the URL {} already",
                row.web_url
            )));
        }
        let (id, created_at): (i64, i64) = tx.query_row(
            "INSERT INTO providers (name, web_url, api_url, git_name, git_email, route, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (name) DO UPDATE SET web_url = excluded.web_url,
                 api_url = excluded.api_url, git_name = excluded.git_name,
                 git_email = excluded.git_email, route = excluded.route
             RETURNING id, created_at",
            params![
                row.name,
                row.web_url,
                row.api_url,
                row.git_name,
                row.git_email,
                row.route.as_str(),
                to_i64(now_unix())
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.commit()?;
        Ok(ProviderRow {
            id: to_u64(id),
            created_at: to_u64(created_at),
            ..row.clone()
        })
    }

    pub fn delete_provider(&self, name: &str) -> StoreResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(id) = tx
            .query_row("SELECT id FROM providers WHERE name = ?1", [name], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        else {
            return Ok(false);
        };
        let id = to_u64(id);
        let credentials = names(
            &tx,
            "SELECT name FROM git_credentials WHERE provider = ?1 ORDER BY name",
            id,
        )?;
        if !credentials.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the Git credentials {} use this provider",
                credentials.join(", ")
            )));
        }
        let workspaces = names(
            &tx,
            "SELECT DISTINCT w.name FROM workspace_repositories r
             JOIN workspaces w ON w.id = r.workspace WHERE r.provider = ?1 ORDER BY w.name",
            id,
        )?;
        if !workspaces.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the workspaces {} use this provider",
                workspaces.join(", ")
            )));
        }
        tx.execute("DELETE FROM providers WHERE id = ?1", [to_i64(id)])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn credentials(&self) -> StoreResult<Vec<CredentialRow>> {
        let rows = self
            .conn()
            .prepare(&format!("{CREDENTIAL_SELECT} ORDER BY c.name"))?
            .query_map([], credential_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn credential_by_name(&self, name: &str) -> StoreResult<Option<CredentialRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("{CREDENTIAL_SELECT} WHERE c.name = ?1"),
                [name],
                credential_row,
            )
            .optional()?)
    }

    pub fn put_credential(
        &self,
        name: &str,
        provider: u64,
        owner: Option<&str>,
        repository: Option<&str>,
        token: Option<&str>,
    ) -> StoreResult<CredentialRow> {
        let sealed = match token {
            Some(token) => Some(
                self.sealer()?
                    .seal(&token_context(name), token.as_bytes())?,
            ),
            None => None,
        };
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let now = to_i64(now_unix());
            let existing: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT id, provider FROM git_credentials WHERE name = ?1",
                    [name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match (existing, sealed) {
                (Some((id, old_provider)), sealed) => {
                    let reset = sealed.is_some() || old_provider != to_i64(provider);
                    tx.execute(
                        "UPDATE git_credentials SET provider = ?1, owner = ?2, repository = ?3,
                             updated_at = ?4 WHERE id = ?5",
                        params![to_i64(provider), owner, repository, now, id],
                    )?;
                    if let Some(sealed) = sealed {
                        tx.execute(
                            "UPDATE git_credentials SET token = ?1 WHERE id = ?2",
                            params![sealed, id],
                        )?;
                    }
                    if reset {
                        tx.execute(
                            "UPDATE git_credentials SET login = NULL, user_id = NULL,
                                 expires_at = NULL, checked_at = NULL, check_error = NULL
                             WHERE id = ?1",
                            [id],
                        )?;
                    }
                }
                (None, Some(sealed)) => {
                    tx.execute(
                        "INSERT INTO git_credentials
                             (name, provider, owner, repository, token, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                        params![name, to_i64(provider), owner, repository, sealed, now],
                    )?;
                }
                (None, None) => {
                    return Err(StoreError::Conflict(format!(
                        "the new Git credential {name} needs a token"
                    )));
                }
            }
            tx.commit()?;
        }
        self.credential_by_name(name)?
            .ok_or(StoreError::Malformed("new Git credential"))
    }

    pub fn credential_token(&self, id: u64) -> StoreResult<Zeroizing<String>> {
        let sealer = self.sealer()?;
        let (name, sealed): (String, Vec<u8>) = self.conn().query_row(
            "SELECT name, token FROM git_credentials WHERE id = ?1",
            [to_i64(id)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let plain = sealer.open(&token_context(&name), &sealed)?;
        let text = std::str::from_utf8(&plain).map_err(|_| StoreError::Malformed("token"))?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    pub fn set_credential_check(&self, id: u64, check: &CredentialCheck) -> StoreResult<()> {
        self.conn().execute(
            "UPDATE git_credentials SET login = ?1, user_id = ?2, expires_at = ?3,
                 checked_at = ?4, check_error = ?5 WHERE id = ?6",
            params![
                check.login,
                check.user_id.map(to_i64),
                check.expires_at.map(to_i64),
                to_i64(now_unix()),
                check.error,
                to_i64(id)
            ],
        )?;
        Ok(())
    }

    pub fn delete_credential(&self, name: &str) -> StoreResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(id) = tx
            .query_row(
                "SELECT id FROM git_credentials WHERE name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        else {
            return Ok(false);
        };
        let presets = names(
            &tx,
            "SELECT DISTINCT p.name FROM preset_git_credentials g
             JOIN presets p ON p.id = g.preset WHERE g.credential = ?1 ORDER BY p.name",
            to_u64(id),
        )?;
        if !presets.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the presets {} use this credential",
                presets.join(", ")
            )));
        }
        tx.execute("DELETE FROM git_credentials WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    fn preset_rows(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> StoreResult<Vec<PresetRow>> {
        let conn = self.conn();
        let mut rows = conn
            .prepare(&format!(
                "SELECT id, name, yaml, created_at, updated_at FROM presets {filter} ORDER BY name"
            ))?
            .query_map(args, |r| {
                Ok(PresetRow {
                    id: to_u64(r.get(0)?),
                    name: r.get(1)?,
                    yaml: r.get(2)?,
                    credentials: Vec::new(),
                    created_at: to_u64(r.get(3)?),
                    updated_at: to_u64(r.get(4)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for row in &mut rows {
            row.credentials = conn
                .prepare(
                    "SELECT c.name FROM preset_git_credentials g
                     JOIN git_credentials c ON c.id = g.credential
                     WHERE g.preset = ?1 ORDER BY g.position",
                )?
                .query_map([to_i64(row.id)], |r| r.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
        }
        Ok(rows)
    }

    pub fn presets(&self) -> StoreResult<Vec<PresetRow>> {
        self.preset_rows("", &[])
    }

    pub fn preset_by_name(&self, name: &str) -> StoreResult<Option<PresetRow>> {
        Ok(self
            .preset_rows("WHERE name = ?1", &[&name])?
            .into_iter()
            .next())
    }

    pub fn preset(&self, id: u64) -> StoreResult<Option<PresetRow>> {
        Ok(self
            .preset_rows("WHERE id = ?1", &[&to_i64(id)])?
            .into_iter()
            .next())
    }

    pub fn put_preset(
        &self,
        name: &str,
        yaml: &str,
        credentials: &[u64],
    ) -> StoreResult<PresetRow> {
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let now = to_i64(now_unix());
            let id: i64 = tx.query_row(
                "INSERT INTO presets (name, yaml, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT (name) DO UPDATE SET yaml = excluded.yaml,
                     updated_at = excluded.updated_at
                 RETURNING id",
                params![name, yaml, now],
                |r| r.get(0),
            )?;
            tx.execute("DELETE FROM preset_git_credentials WHERE preset = ?1", [id])?;
            for (position, credential) in credentials.iter().enumerate() {
                tx.execute(
                    "INSERT INTO preset_git_credentials (preset, position, credential)
                     VALUES (?1, ?2, ?3)",
                    params![id, position as i64, to_i64(*credential)],
                )?;
            }
            tx.commit()?;
        }
        self.preset_by_name(name)?
            .ok_or(StoreError::Malformed("new preset"))
    }

    pub fn delete_preset(&self, name: &str) -> StoreResult<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let Some(id) = tx
            .query_row("SELECT id FROM presets WHERE name = ?1", [name], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
        else {
            return Ok(false);
        };
        let open = tx
            .prepare("SELECT id FROM codespaces WHERE preset = ?1 AND state != ?2 ORDER BY id")?
            .query_map(params![id, CodespaceState::Closed.as_str()], |r| r.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        if !open.is_empty() {
            return Err(StoreError::Conflict(format!(
                "the codespaces {} use this preset",
                open.join(", ")
            )));
        }
        tx.execute("DELETE FROM presets WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn preset_credentials(&self, preset: u64) -> StoreResult<Vec<CredentialRow>> {
        let rows = self
            .conn()
            .prepare(&format!(
                "{CREDENTIAL_SELECT} JOIN preset_git_credentials g ON g.credential = c.id
                 WHERE g.preset = ?1 ORDER BY g.position"
            ))?
            .query_map([to_i64(preset)], credential_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn presets_with_credential(&self, credential: u64) -> StoreResult<Vec<u64>> {
        let ids = self
            .conn()
            .prepare("SELECT DISTINCT preset FROM preset_git_credentials WHERE credential = ?1")?
            .query_map([to_i64(credential)], |r| r.get::<_, i64>(0))?
            .map(|id| id.map(to_u64))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use loom_oracle_proto::{Repository, Role};

    use super::*;
    use crate::seal::MasterKey;

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.unlock(&MasterKey::generate()).unwrap();
        store
    }

    fn provider(name: &str, web_url: &str) -> ProviderRow {
        ProviderRow {
            id: 0,
            name: name.into(),
            web_url: web_url.into(),
            api_url: format!("{web_url}/api/v3"),
            git_name: None,
            git_email: None,
            route: ProviderRoute::Direct,
            created_at: 0,
        }
    }

    #[test]
    fn providers_have_unique_names_and_urls() {
        let store = store();
        let ghe = store
            .put_provider(&provider("ghe", "https://ghe.example.com"))
            .unwrap();
        assert!(ghe.id > 0);
        let changed = store
            .put_provider(&ProviderRow {
                route: ProviderRoute::Hosts,
                git_name: Some("Me".into()),
                ..provider("ghe", "https://ghe.example.com")
            })
            .unwrap();
        assert_eq!(changed.id, ghe.id);
        assert_eq!(changed.created_at, ghe.created_at);
        let stored = store.provider_by_name("ghe").unwrap().unwrap();
        assert_eq!(stored.route, ProviderRoute::Hosts);
        assert_eq!(stored.git_name.as_deref(), Some("Me"));
        assert!(matches!(
            store.put_provider(&provider("other", "https://ghe.example.com")),
            Err(StoreError::Conflict(_))
        ));
        assert!(store.delete_provider("ghe").unwrap());
        assert!(!store.delete_provider("ghe").unwrap());
    }

    #[test]
    fn tokens_are_sealed_and_presets_keep_their_order() {
        let store = store();
        let ghe = store
            .put_provider(&provider("ghe", "https://ghe.example.com"))
            .unwrap();
        assert!(matches!(
            store.put_credential("work", ghe.id, None, None, None),
            Err(StoreError::Conflict(_))
        ));
        let work = store
            .put_credential("work", ghe.id, Some("acme"), None, Some("ghp_first"))
            .unwrap();
        assert_eq!(work.provider_name, "ghe");
        assert_eq!(work.owner.as_deref(), Some("acme"));
        let sealed: Vec<u8> = store
            .conn()
            .query_row("SELECT token FROM git_credentials", [], |r| r.get(0))
            .unwrap();
        assert!(!sealed.windows(9).any(|w| w == b"ghp_first"));
        assert_eq!(
            store.credential_token(work.id).unwrap().as_str(),
            "ghp_first"
        );

        store
            .set_credential_check(
                work.id,
                &CredentialCheck {
                    login: Some("me".into()),
                    user_id: Some(7),
                    expires_at: Some(99),
                    error: None,
                },
            )
            .unwrap();
        let checked = store.credential_by_name("work").unwrap().unwrap();
        assert_eq!(checked.login.as_deref(), Some("me"));
        assert!(checked.checked_at.is_some());
        let renamed = store
            .put_credential("work", ghe.id, Some("acme"), Some("server"), None)
            .unwrap();
        assert_eq!(renamed.login.as_deref(), Some("me"));
        assert_eq!(
            store.credential_token(work.id).unwrap().as_str(),
            "ghp_first"
        );
        let replaced = store
            .put_credential("work", ghe.id, Some("acme"), None, Some("ghp_second"))
            .unwrap();
        assert_eq!(replaced.login, None);
        assert_eq!(
            store.credential_token(work.id).unwrap().as_str(),
            "ghp_second"
        );

        let all = store
            .put_credential("all", ghe.id, None, None, Some("ghp_all"))
            .unwrap();
        let preset = store
            .put_preset("dev", "git: {}\n", &[all.id, work.id])
            .unwrap();
        assert_eq!(
            preset.credentials,
            vec!["all".to_owned(), "work".to_owned()]
        );
        let ordered: Vec<String> = store
            .preset_credentials(preset.id)
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(ordered, vec!["all".to_owned(), "work".to_owned()]);
        assert_eq!(
            store.presets_with_credential(work.id).unwrap(),
            vec![preset.id]
        );
        assert!(matches!(
            store.delete_credential("work"),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.delete_provider("ghe"),
            Err(StoreError::Conflict(_))
        ));
        let again = store.put_preset("dev", "", &[all.id]).unwrap();
        assert_eq!(again.id, preset.id);
        assert_eq!(again.credentials, vec!["all".to_owned()]);
        assert!(store.delete_credential("work").unwrap());

        let (host, _) = store.add_peer(Role::Host, "box", &[1; 32], None).unwrap();
        let repository = Repository {
            url: "https://ghe.example.com/acme/server.git".into(),
            provider: Some("ghe".into()),
            directory: Some("server".into()),
            base_branch: None,
        };
        let workspace = store
            .create_workspace("acme", host.id, std::slice::from_ref(&repository), &[])
            .unwrap();
        assert_eq!(workspace.repositories, vec![repository]);
        assert!(store.delete_preset("dev").unwrap());
        assert!(store.preset_by_name("dev").unwrap().is_none());
        assert!(matches!(
            store.delete_provider("ghe"),
            Err(StoreError::Conflict(_))
        ));
    }
}
