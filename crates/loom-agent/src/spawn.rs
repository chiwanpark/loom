use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use loom_config::Dirs;
use loom_transport::{AgentEntry, Registry};

pub const EXIT_ALREADY_RUNNING: i32 = 3;
const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);

pub fn log_path(dirs: &Dirs, workspace: &Path) -> PathBuf {
    let name: String = workspace
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = if name.is_empty() {
        "root".to_owned()
    } else {
        name
    };
    let tail: String = name
        .chars()
        .rev()
        .take(100)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    dirs.state_home
        .join(loom_config::APP_DIR)
        .join("logs")
        .join(format!("agent-{tail}.log"))
}

pub async fn spawn_agent(
    exe: &Path,
    config_path: &Path,
    workspace: &Path,
    registry: &Registry,
    dirs: &Dirs,
) -> anyhow::Result<AgentEntry> {
    let log = log_path(dirs, workspace);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log_file = OpenOptions::new().create(true).append(true).open(&log)?;
    let mut command = Command::new(exe);
    command
        .arg("serve")
        .arg("--workspace")
        .arg(workspace)
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file))
        .process_group(0);
    let mut child = command.spawn()?;
    tracing::info!(pid = child.id(), workspace = %workspace.display(), "spawned an agent");
    let entry = registry
        .wait_for_workspace(workspace, SPAWN_TIMEOUT, || match child.try_wait() {
            Ok(Some(status)) if status.code() == Some(EXIT_ALREADY_RUNNING) => None,
            Ok(Some(status)) => Some(format!(
                "the agent exited during startup ({status}); see {}",
                log.display()
            )),
            Ok(None) => None,
            Err(err) => Some(err.to_string()),
        })
        .await?;
    Ok(entry)
}
