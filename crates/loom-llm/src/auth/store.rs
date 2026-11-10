use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{LlmError, Result};

pub const CREDENTIALS_FILE: &str = "credentials.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub version: u32,
    #[serde(default)]
    pub openai: OpenAiCredentials,
    #[serde(
        default,
        rename = "openai-codex",
        skip_serializing_if = "Option::is_none"
    )]
    pub openai_codex: Option<CodexCredential>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            version: VERSION,
            openai: OpenAiCredentials::default(),
            openai_codex: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCredentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub accounts: Vec<ChatGptAccount>,
}

impl OpenAiCredentials {
    pub fn active_account(&self) -> Option<&ChatGptAccount> {
        let active = self.active.as_deref()?;
        self.accounts.iter().find(|a| a.client_id == active)
    }

    pub fn account_mut(&mut self, client_id: &str) -> Option<&mut ChatGptAccount> {
        self.accounts.iter_mut().find(|a| a.client_id == client_id)
    }

    pub fn upsert(&mut self, account: ChatGptAccount) {
        let client_id = account.client_id.clone();
        match self.account_mut(&client_id) {
            Some(existing) => *existing = account,
            None => self.accounts.push(account),
        }
        self.active = Some(client_id);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptAccount {
    pub client_id: String,
    pub issuer: String,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<OAuthTokens>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub saved_at: u64,
}

impl OAuthTokens {
    pub fn is_fresh(&self, margin_secs: u64) -> bool {
        self.expires_at > now_unix() + margin_secs
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCredential {
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub saved_at: u64,
}

impl CodexCredential {
    pub fn is_fresh(&self, margin_secs: u64) -> bool {
        self.expires_at > now_unix() + margin_secs
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

pub struct StoreGuard {
    path: PathBuf,
    _lock: File,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn in_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join(CREDENTIALS_FILE))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn load(&self) -> Result<Credentials> {
        let path = self.path.clone();
        blocking(move || read_credentials(&path)).await
    }

    pub async fn lock(&self) -> Result<StoreGuard> {
        let path = self.path.clone();
        blocking(move || {
            let lock_path = lock_path(&path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|source| LlmError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(&lock_path)
                .map_err(|source| LlmError::Io {
                    path: lock_path.clone(),
                    source,
                })?;
            file.lock().map_err(|source| LlmError::Io {
                path: lock_path,
                source,
            })?;
            Ok(StoreGuard { path, _lock: file })
        })
        .await
    }

    pub async fn update<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Credentials) -> R + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.lock().await?;
        blocking(move || {
            let mut creds = read_credentials(&guard.path)?;
            let result = f(&mut creds);
            write_credentials(&guard.path, &creds)?;
            drop(guard);
            Ok(result)
        })
        .await
    }

    pub async fn host_id(&self) -> Result<String> {
        if let Some(id) = self.load().await?.openai.host_id {
            return Ok(id);
        }
        self.update(|creds| {
            creds
                .openai
                .host_id
                .get_or_insert_with(|| format!("urn:uuid:{}", uuid::Uuid::new_v4()))
                .clone()
        })
        .await
    }
}

impl StoreGuard {
    pub async fn load(&self) -> Result<Credentials> {
        let path = self.path.clone();
        blocking(move || read_credentials(&path)).await
    }

    pub async fn save(&self, creds: Credentials) -> Result<()> {
        let path = self.path.clone();
        blocking(move || write_credentials(&path, &creds)).await
    }
}

async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| LlmError::Store {
            path: PathBuf::new(),
            message: e.to_string(),
        })?
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

fn read_credentials(path: &Path) -> Result<Credentials> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Credentials::default()),
        Err(source) => {
            return Err(LlmError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if text.trim().is_empty() {
        return Ok(Credentials::default());
    }
    let creds: Credentials = serde_json::from_str(&text).map_err(|e| LlmError::Store {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    if creds.version > VERSION {
        return Err(LlmError::Store {
            path: path.to_path_buf(),
            message: format!("unsupported version {}", creds.version),
        });
    }
    Ok(creds)
}

fn write_credentials(path: &Path, creds: &Credentials) -> Result<()> {
    let io_err = |source| LlmError::Io {
        path: path.to_path_buf(),
        source,
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_err)?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(io_err)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(io_err)?;
    let json = serde_json::to_vec_pretty(creds).map_err(|e| LlmError::Store {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    file.write_all(&json).map_err(io_err)?;
    file.as_file().sync_all().map_err(io_err)?;
    file.persist(path).map_err(|e| io_err(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(client_id: &str) -> ChatGptAccount {
        ChatGptAccount {
            client_id: client_id.into(),
            issuer: "https://auth.openai.com".into(),
            subject: "sub".into(),
            email: None,
            id_token: None,
            tokens: None,
        }
    }

    #[tokio::test]
    async fn missing_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        assert_eq!(store.load().await.unwrap(), Credentials::default());
    }

    #[tokio::test]
    async fn update_writes_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(&dir.path().join("nested"));
        store
            .update(|c| c.openai.api_key = Some("sk-test".into()))
            .await
            .unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let creds = store.load().await.unwrap();
        assert_eq!(creds.openai.api_key.as_deref(), Some("sk-test"));
    }

    #[tokio::test]
    async fn host_id_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        let first = store.host_id().await.unwrap();
        assert!(first.starts_with("urn:uuid:"));
        assert_eq!(store.host_id().await.unwrap(), first);
    }

    #[tokio::test]
    async fn upsert_replaces_and_activates() {
        let mut creds = OpenAiCredentials::default();
        creds.upsert(account("a"));
        creds.upsert(account("b"));
        let mut replaced = account("a");
        replaced.email = Some("x@example.com".into());
        creds.upsert(replaced);
        assert_eq!(creds.accounts.len(), 2);
        assert_eq!(
            creds.active_account().unwrap().email.as_deref(),
            Some("x@example.com")
        );
    }

    #[tokio::test]
    async fn concurrent_updates_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        let mut tasks = Vec::new();
        for i in 0..8 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .update(move |c| c.openai.accounts.push(account(&i.to_string())))
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(store.load().await.unwrap().openai.accounts.len(), 8);
    }
}
