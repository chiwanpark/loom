use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const AGENTS_DIR: &str = "agents";
const WORKSPACES_DIR: &str = "workspaces";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEntry {
    pub instance: String,
    pub pid: u32,
    pub workspace: PathBuf,
    pub socket: PathBuf,
    pub started_at: u64,
    pub version: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("an agent is already running for {0}")]
    AlreadyRunning(PathBuf),
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct Registry {
    root: PathBuf,
}

pub struct Registration {
    entry: Option<AgentEntry>,
    agents: PathBuf,
    instance: String,
    _instance_lock: File,
    _workspace_lock: File,
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
}

fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn workspace_key(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

fn remove_quietly(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(path = %path.display(), error = %err, "could not remove registry file");
    }
}

impl Registry {
    pub fn new(runtime_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: runtime_dir.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn agents_dir(&self) -> PathBuf {
        self.root.join(AGENTS_DIR)
    }

    pub fn socket_path(&self, instance: &str) -> PathBuf {
        self.agents_dir().join(format!("{instance}.sock"))
    }

    fn entry_path(&self, instance: &str) -> PathBuf {
        self.agents_dir().join(format!("{instance}.json"))
    }

    fn lock_path(&self, instance: &str) -> PathBuf {
        self.agents_dir().join(format!("{instance}.lock"))
    }

    pub fn ensure_dirs(&self) -> io::Result<()> {
        create_private_dir(&self.root)?;
        create_private_dir(&self.agents_dir())?;
        create_private_dir(&self.root.join(WORKSPACES_DIR))
    }

    pub fn claim(&self, instance: &str, workspace: &Path) -> Result<Registration, RegistryError> {
        self.ensure_dirs()?;
        let workspace_lock = open_lock(
            &self
                .root
                .join(WORKSPACES_DIR)
                .join(format!("{}.lock", workspace_key(workspace))),
        )?;
        if !try_lock(&workspace_lock)? {
            return Err(RegistryError::AlreadyRunning(workspace.to_path_buf()));
        }
        let instance_lock = open_lock(&self.lock_path(instance))?;
        if !try_lock(&instance_lock)? {
            return Err(RegistryError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("instance {instance} is already registered"),
            )));
        }
        Ok(Registration {
            entry: None,
            agents: self.agents_dir(),
            instance: instance.to_owned(),
            _instance_lock: instance_lock,
            _workspace_lock: workspace_lock,
        })
    }

    pub fn list(&self) -> io::Result<Vec<AgentEntry>> {
        let dir = match std::fs::read_dir(self.agents_dir()) {
            Ok(dir) => dir,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let mut entries = Vec::new();
        for item in dir {
            let path = item?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(instance) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !self.is_alive(instance)? {
                self.remove_stale(instance);
                continue;
            }
            match std::fs::read(&path).map(|bytes| serde_json::from_slice::<AgentEntry>(&bytes)) {
                Ok(Ok(entry)) => entries.push(entry),
                Ok(Err(err)) => {
                    tracing::warn!(path = %path.display(), error = %err, "skipping a malformed registry entry")
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
        entries.sort_by_key(|e| (e.started_at, e.instance.clone()));
        Ok(entries)
    }

    fn is_alive(&self, instance: &str) -> io::Result<bool> {
        let file = match File::open(self.lock_path(instance)) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        Ok(!try_lock(&file)?)
    }

    fn remove_stale(&self, instance: &str) {
        remove_quietly(&self.entry_path(instance));
        remove_quietly(&self.socket_path(instance));
        remove_quietly(&self.lock_path(instance));
    }

    pub fn find_workspace(&self, workspace: &Path) -> io::Result<Option<AgentEntry>> {
        Ok(self.list()?.into_iter().find(|e| e.workspace == workspace))
    }

    pub fn find_instance(&self, instance: &str) -> io::Result<Option<AgentEntry>> {
        Ok(self.list()?.into_iter().find(|e| e.instance == instance))
    }

    pub async fn wait_for_workspace(
        &self,
        workspace: &Path,
        timeout: Duration,
        mut give_up: impl FnMut() -> Option<String>,
    ) -> io::Result<AgentEntry> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(entry) = self.find_workspace(workspace)? {
                return Ok(entry);
            }
            if let Some(reason) = give_up() {
                return Err(io::Error::other(reason));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no agent registered for {} in time", workspace.display()),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn wait_until_gone(&self, instance: &str, timeout: Duration) -> io::Result<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.is_alive(instance)? {
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.remove_stale(instance);
        Ok(true)
    }
}

impl Registration {
    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn publish(&mut self, entry: AgentEntry) -> io::Result<()> {
        let path = self.agents.join(format!("{}.json", self.instance));
        let tmp = self.agents.join(format!(".{}.json.tmp", self.instance));
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&entry).map_err(io::Error::other)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        self.entry = Some(entry);
        Ok(())
    }

    pub fn entry(&self) -> Option<&AgentEntry> {
        self.entry.as_ref()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        remove_quietly(&self.agents.join(format!("{}.json", self.instance)));
        remove_quietly(&self.agents.join(format!("{}.sock", self.instance)));
        remove_quietly(&self.agents.join(format!("{}.lock", self.instance)));
    }
}
