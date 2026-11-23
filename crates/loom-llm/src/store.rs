use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{LlmError, Result};

pub const CREDENTIALS_FILE: &str = "credentials.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub version: u32,
    #[serde(flatten)]
    pub sections: BTreeMap<String, Value>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            version: VERSION,
            sections: BTreeMap::new(),
        }
    }
}

impl Credentials {
    pub fn section(&self, provider: &str) -> Option<&Value> {
        self.sections.get(provider)
    }

    pub fn set_section(&mut self, provider: &str, value: Option<Value>) {
        match value {
            Some(value) => {
                self.sections.insert(provider.to_owned(), value);
            }
            None => {
                self.sections.remove(provider);
            }
        }
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    fn location(&self) -> PathBuf;

    async fn load(&self) -> Result<Credentials>;

    async fn lock(&self) -> Result<Box<dyn CredentialLock>>;

    fn read_only(&self) -> Option<String> {
        None
    }

    async fn account_names(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

#[async_trait]
pub trait CredentialLock: Send + Sync {
    async fn load(&self) -> Result<Credentials>;

    async fn save(&self, credentials: Credentials) -> Result<()>;
}

struct FileSource {
    path: PathBuf,
}

struct FileLock {
    path: PathBuf,
    _lock: File,
}

#[async_trait]
impl CredentialLock for FileLock {
    async fn load(&self) -> Result<Credentials> {
        let path = self.path.clone();
        blocking(move || read_credentials(&path)).await
    }

    async fn save(&self, credentials: Credentials) -> Result<()> {
        let path = self.path.clone();
        blocking(move || write_credentials(&path, &credentials)).await
    }
}

#[async_trait]
impl CredentialSource for FileSource {
    fn location(&self) -> PathBuf {
        self.path.clone()
    }

    async fn load(&self) -> Result<Credentials> {
        let path = self.path.clone();
        blocking(move || read_credentials(&path)).await
    }

    async fn lock(&self) -> Result<Box<dyn CredentialLock>> {
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
            Ok(Box::new(FileLock { path, _lock: file }) as Box<dyn CredentialLock>)
        })
        .await
    }
}

#[derive(Clone)]
pub struct CredentialStore {
    source: Arc<dyn CredentialSource>,
    path: PathBuf,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("path", &self.path)
            .finish()
    }
}

pub struct StoreGuard {
    path: PathBuf,
    lock: Box<dyn CredentialLock>,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self {
            source: Arc::new(FileSource { path: path.clone() }),
            path,
        }
    }

    pub fn in_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join(CREDENTIALS_FILE))
    }

    pub fn from_source(source: Arc<dyn CredentialSource>) -> Self {
        Self {
            path: source.location(),
            source,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_only(&self) -> Option<String> {
        self.source.read_only()
    }

    pub async fn account_names(&self) -> BTreeMap<String, String> {
        self.source.account_names().await
    }

    pub async fn load(&self) -> Result<Credentials> {
        self.source.load().await
    }

    pub async fn lock(&self) -> Result<StoreGuard> {
        Ok(StoreGuard {
            path: self.path.clone(),
            lock: self.source.lock().await?,
        })
    }

    pub async fn update<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Credentials) -> R + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.lock().await?;
        let mut creds = guard.load().await?;
        let result = f(&mut creds);
        guard.save(creds).await?;
        Ok(result)
    }

    pub(crate) async fn load_section<T: DeserializeOwned>(
        &self,
        provider: &str,
    ) -> Result<Option<T>> {
        let creds = self.load().await?;
        decode_section(&self.path, &creds, provider)
    }

    pub(crate) async fn update_section<T, R, F>(&self, provider: &str, f: F) -> Result<R>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(&mut Option<T>) -> R + Send + 'static,
        R: Send + 'static,
    {
        let path = self.path.clone();
        let provider = provider.to_owned();
        self.update(move |creds| {
            let mut section = decode_section::<T>(&path, creds, &provider)?;
            let result = f(&mut section);
            creds.set_section(&provider, encode_section(&path, section.as_ref())?);
            Ok(result)
        })
        .await?
    }
}

impl StoreGuard {
    pub async fn load(&self) -> Result<Credentials> {
        self.lock.load().await
    }

    pub async fn save(&self, creds: Credentials) -> Result<()> {
        self.lock.save(creds).await
    }

    pub(crate) async fn load_section<T: DeserializeOwned>(
        &self,
        provider: &str,
    ) -> Result<Option<T>> {
        let creds = self.load().await?;
        decode_section(&self.path, &creds, provider)
    }

    pub(crate) async fn save_section<T: Serialize>(
        &self,
        provider: &str,
        section: Option<&T>,
    ) -> Result<()> {
        let mut creds = self.load().await?;
        creds.set_section(provider, encode_section(&self.path, section)?);
        self.save(creds).await
    }
}

fn store_error(path: &Path, message: impl Into<String>) -> LlmError {
    LlmError::Store {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn decode_section<T: DeserializeOwned>(
    path: &Path,
    creds: &Credentials,
    provider: &str,
) -> Result<Option<T>> {
    creds
        .section(provider)
        .map(|value| {
            serde_json::from_value(value.clone())
                .map_err(|e| store_error(path, format!("invalid {provider} section: {e}")))
        })
        .transpose()
}

fn encode_section<T: Serialize>(path: &Path, section: Option<&T>) -> Result<Option<Value>> {
    section
        .map(|s| serde_json::to_value(s).map_err(|e| store_error(path, e.to_string())))
        .transpose()
}

async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| store_error(Path::new(""), e.to_string()))?
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
    let creds: Credentials =
        serde_json::from_str(&text).map_err(|e| store_error(path, e.to_string()))?;
    if creds.version > VERSION {
        return Err(store_error(
            path,
            format!("unsupported version {}", creds.version),
        ));
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
    let json = serde_json::to_vec_pretty(creds).map_err(|e| store_error(path, e.to_string()))?;
    file.write_all(&json).map_err(io_err)?;
    file.as_file().sync_all().map_err(io_err)?;
    file.persist(path).map_err(|e| io_err(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Section {
        token: String,
    }

    #[tokio::test]
    async fn missing_file_loads_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        assert_eq!(store.load().await.unwrap(), Credentials::default());
    }

    #[tokio::test]
    async fn sections_are_stored_at_top_level_with_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(&dir.path().join("nested"));
        store
            .update_section::<Section, _, _>("alpha", |s| *s = Some(Section { token: "t".into() }))
            .await
            .unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(raw, json!({"version": 1, "alpha": {"token": "t"}}));
        let loaded: Option<Section> = store.load_section("alpha").await.unwrap();
        assert_eq!(loaded.unwrap().token, "t");
        store
            .update_section::<Section, _, _>("alpha", |s| *s = None)
            .await
            .unwrap();
        assert!(store.load().await.unwrap().sections.is_empty());
    }

    #[tokio::test]
    async fn invalid_sections_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        store
            .update(|c| c.set_section("alpha", Some(json!({"wrong": 1}))))
            .await
            .unwrap();
        let err = store.load_section::<Section>("alpha").await.unwrap_err();
        assert!(err.to_string().contains("invalid alpha section"));
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
                    .update(move |c| c.set_section(&format!("p{i}"), Some(json!(i))))
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(store.load().await.unwrap().sections.len(), 8);
    }
}
