use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use loom_config::{AgentConfig, Dirs};
use loom_transport::{AgentEntry, Registry};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::spawn::spawn_agent;
use crate::workspace::{canonical_workspace, resolve_workspace};

const MANAGED_WAIT: Duration = Duration::from_secs(30);
const MANAGED_POLL: Duration = Duration::from_millis(250);

pub enum StdioTarget {
    Workspace { path: PathBuf, spawn: bool },
    Instance(String),
    Managed,
}

pub async fn find_managed(registry: &Registry, wait: Duration) -> anyhow::Result<AgentEntry> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if let Some(entry) = registry
            .list()?
            .into_iter()
            .max_by_key(|entry| entry.started_at)
        {
            return Ok(entry);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the agent of this codespace is not running; check `systemctl status loom-agent`"
            );
        }
        tokio::time::sleep(MANAGED_POLL).await;
    }
}

pub async fn find_target(
    target: &StdioTarget,
    config_path: &Path,
    config: &AgentConfig,
    dirs: &Dirs,
) -> anyhow::Result<AgentEntry> {
    let registry = Registry::new(config.runtime_dir(dirs));
    match target {
        StdioTarget::Managed => find_managed(&registry, MANAGED_WAIT).await,
        StdioTarget::Instance(id) => registry
            .find_instance(id)?
            .with_context(|| format!("no agent with instance {id} is running")),
        StdioTarget::Workspace { path, spawn } => {
            let workspace = canonical_workspace(path, dirs)?;
            if let Some(entry) = registry.find_workspace(&workspace)? {
                return Ok(entry);
            }
            if !*spawn {
                bail!(
                    "no agent is running for {}; start one with `loom-agent serve --workspace {}` or pass --spawn",
                    workspace.display(),
                    workspace.display()
                );
            }
            let workspace = resolve_workspace(Some(&workspace), config, dirs)?;
            let exe = std::env::current_exe().context("cannot locate the loom-agent binary")?;
            spawn_agent(&exe, config_path, &workspace, &registry, dirs).await
        }
    }
}

pub async fn bridge(entry: &AgentEntry) -> anyhow::Result<()> {
    let socket = UnixStream::connect(&entry.socket)
        .await
        .with_context(|| format!("could not connect to {}", entry.socket.display()))?;
    let (mut from_agent, mut to_agent) = socket.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let upstream = async {
        let result = tokio::io::copy(&mut stdin, &mut to_agent).await;
        let _ = to_agent.shutdown().await;
        result
    };
    let downstream = async {
        let result = tokio::io::copy(&mut from_agent, &mut stdout).await;
        let _ = stdout.flush().await;
        result
    };
    tokio::pin!(upstream);
    tokio::pin!(downstream);
    tokio::select! {
        result = &mut downstream => {
            result.context("agent connection failed")?;
        }
        result = &mut upstream => {
            result.context("client connection failed")?;
            downstream.await.context("agent connection failed")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_managed_target_is_the_newest_agent() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path());
        let err = find_managed(&registry, Duration::ZERO).await.unwrap_err();
        assert!(err.to_string().contains("not running"), "{err}");
        let mut registrations = Vec::new();
        for (instance, started_at) in [("a", 10), ("b", 20)] {
            let workspace = dir.path().join(instance);
            let mut registration = registry.claim(instance, &workspace).unwrap();
            registration
                .publish(AgentEntry {
                    instance: instance.into(),
                    pid: std::process::id(),
                    workspace,
                    socket: dir.path().join(format!("{instance}.sock")),
                    started_at,
                    version: "test".into(),
                })
                .unwrap();
            registrations.push(registration);
        }
        let entry = find_managed(&registry, Duration::ZERO).await.unwrap();
        assert_eq!(entry.instance, "b");
    }
}
