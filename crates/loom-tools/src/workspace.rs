use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use crate::error::{Result, ToolError};

#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    locks: LockManager,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let root = root.canonicalize().map_err(|err| {
            ToolError::io(format!("cannot open workspace {}", root.display()), err)
        })?;
        if !root.is_dir() {
            return Err(ToolError::failed(format!(
                "workspace {} is not a directory",
                root.display()
            )));
        }
        Ok(Self {
            root,
            locks: LockManager::default(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn locks(&self) -> &LockManager {
        &self.locks
    }

    pub fn resolve(&self, input: &str) -> Result<PathBuf> {
        if input.trim().is_empty() {
            return Err(ToolError::InvalidArguments("path must not be empty".into()));
        }
        let expanded = expand_home(input);
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.root.join(expanded)
        };
        let normalized = normalize_lexically(&joined);
        let resolved = resolve_existing_prefix(&normalized);
        if resolved.starts_with(&self.root) {
            Ok(resolved)
        } else {
            Err(ToolError::OutsideWorkspace(normalized))
        }
    }

    pub fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

fn expand_home(input: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (input, home) {
        ("~", Some(home)) => home,
        (rest, Some(home)) if rest.starts_with("~/") => home.join(&rest[2..]),
        (rest, _) => PathBuf::from(rest),
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let mut suffix = Vec::new();
    let mut current = path;
    loop {
        if let Ok(canonical) = current.canonicalize() {
            let mut resolved = canonical;
            for part in suffix.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                suffix.push(name.to_owned());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[derive(Debug)]
pub struct LockManager {
    workspace: Arc<RwLock<()>>,
    files: Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>,
}

impl Default for LockManager {
    fn default() -> Self {
        Self {
            workspace: Arc::new(RwLock::new(())),
            files: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAccess {
    Read,
    Write,
}

#[derive(Debug)]
pub struct FileGuard {
    _file: FileLockGuard,
    _workspace: OwnedRwLockReadGuard<()>,
}

#[derive(Debug)]
enum FileLockGuard {
    Read(#[allow(dead_code)] OwnedRwLockReadGuard<()>),
    Write(#[allow(dead_code)] OwnedRwLockWriteGuard<()>),
}

#[derive(Debug)]
pub struct WorkspaceGuard {
    _workspace: OwnedRwLockWriteGuard<()>,
}

impl LockManager {
    pub async fn lock_file(&self, path: &Path, access: FileAccess) -> FileGuard {
        let workspace = self.workspace.clone().read_owned().await;
        let lock = self.file_lock(path);
        let file = match access {
            FileAccess::Read => FileLockGuard::Read(lock.read_owned().await),
            FileAccess::Write => FileLockGuard::Write(lock.write_owned().await),
        };
        FileGuard {
            _file: file,
            _workspace: workspace,
        }
    }

    pub async fn lock_workspace(&self) -> WorkspaceGuard {
        WorkspaceGuard {
            _workspace: self.workspace.clone().write_owned().await,
        }
    }

    pub fn try_lock_file(&self, path: &Path, access: FileAccess) -> Option<FileGuard> {
        let workspace = self.workspace.clone().try_read_owned().ok()?;
        let lock = self.file_lock(path);
        let file = match access {
            FileAccess::Read => FileLockGuard::Read(lock.try_read_owned().ok()?),
            FileAccess::Write => FileLockGuard::Write(lock.try_write_owned().ok()?),
        };
        Some(FileGuard {
            _file: file,
            _workspace: workspace,
        })
    }

    pub fn try_lock_workspace(&self) -> Option<WorkspaceGuard> {
        Some(WorkspaceGuard {
            _workspace: self.workspace.clone().try_write_owned().ok()?,
        })
    }

    fn file_lock(&self, path: &Path) -> Arc<RwLock<()>> {
        let mut files = self.files.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(lock) = files.get(path).and_then(Weak::upgrade) {
            return lock;
        }
        files.retain(|_, lock| lock.strong_count() > 0);
        let lock = Arc::new(RwLock::new(()));
        files.insert(path.to_path_buf(), Arc::downgrade(&lock));
        lock
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FileTracker {
    stamps: Mutex<HashMap<PathBuf, FileStamp>>,
}

impl FileTracker {
    pub fn record(&self, path: &Path, stamp: FileStamp) {
        self.stamps
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(path.to_path_buf(), stamp);
    }

    pub fn forget(&self, path: &Path) {
        self.stamps
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(path);
    }

    /// Forgets every file, as if none had been read.
    pub fn clear(&self) {
        self.stamps
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clear();
    }

    pub fn ensure_fresh(&self, path: &Path, current: Option<FileStamp>) -> Result<()> {
        let stamps = self.stamps.lock().unwrap_or_else(|err| err.into_inner());
        match (stamps.get(path), current) {
            (Some(recorded), Some(current)) if *recorded != current => Err(ToolError::StaleFile {
                path: path.to_path_buf(),
            }),
            (Some(_), None) => Err(ToolError::StaleFile {
                path: path.to_path_buf(),
            }),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn resolves_relative_paths_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let resolved = workspace.resolve("src/../lib.rs").unwrap();
        assert_eq!(resolved, workspace.root().join("lib.rs"));
        assert_eq!(workspace.display_path(&resolved), "lib.rs");
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        assert!(matches!(
            workspace.resolve("../escape.txt"),
            Err(ToolError::OutsideWorkspace(_))
        ));
        assert!(matches!(
            workspace.resolve("/etc/passwd"),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn rejects_symlinks_pointing_outside_workspace() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        assert!(matches!(
            workspace.resolve("link/file.txt"),
            Err(ToolError::OutsideWorkspace(_))
        ));
    }

    #[test]
    fn accepts_absolute_paths_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let absolute = workspace.root().join("a/b.txt");
        assert_eq!(
            workspace.resolve(absolute.to_str().unwrap()).unwrap(),
            absolute
        );
    }

    #[tokio::test]
    async fn workspace_lock_waits_for_file_locks() {
        let locks = Arc::new(LockManager::default());
        let file_guard = locks.lock_file(Path::new("/a"), FileAccess::Write).await;
        let waiter = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _guard = locks.lock_workspace().await;
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        drop(file_guard);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn readers_share_and_writers_exclude() {
        let locks = Arc::new(LockManager::default());
        let path = Path::new("/a");
        let first = locks.lock_file(path, FileAccess::Read).await;
        let second = tokio::time::timeout(
            Duration::from_millis(100),
            locks.lock_file(path, FileAccess::Read),
        )
        .await
        .unwrap();
        let writer = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _guard = locks.lock_file(Path::new("/a"), FileAccess::Write).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!writer.is_finished());
        drop(first);
        drop(second);
        tokio::time::timeout(Duration::from_secs(1), writer)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn tracker_detects_changes() {
        let tracker = FileTracker::default();
        let path = Path::new("/a");
        let stamp = FileStamp {
            len: 1,
            modified: None,
        };
        assert!(tracker.ensure_fresh(path, Some(stamp)).is_ok());
        tracker.record(path, stamp);
        assert!(tracker.ensure_fresh(path, Some(stamp)).is_ok());
        let changed = FileStamp {
            len: 2,
            modified: None,
        };
        assert!(matches!(
            tracker.ensure_fresh(path, Some(changed)),
            Err(ToolError::StaleFile { .. })
        ));
        assert!(matches!(
            tracker.ensure_fresh(path, None),
            Err(ToolError::StaleFile { .. })
        ));
    }
}
