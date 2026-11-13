use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_config::Dirs;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent::Agent;

pub const DEBOUNCE: Duration = Duration::from_millis(200);

pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn watch_config(agent: Agent, path: PathBuf, dirs: Dirs) -> notify::Result<ConfigWatcher> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let name = path.file_name().map(|n| n.to_os_string());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let relevant = event
            .paths
            .iter()
            .any(|p| p.file_name().map(|n| n.to_os_string()) == name);
        if relevant {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;

    let task = tokio::spawn(async move {
        while rx.recv().await.is_some() {
            tokio::time::sleep(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}
            reload(&agent, &path, &dirs).await;
        }
    });
    Ok(ConfigWatcher {
        _watcher: watcher,
        task,
    })
}

/// Reloads after a change of the file; the agent logs and announces the
/// outcome itself.
pub async fn reload(agent: &Agent, path: &Path, dirs: &Dirs) {
    let _ = agent.reload_config_from(path, dirs).await;
}
