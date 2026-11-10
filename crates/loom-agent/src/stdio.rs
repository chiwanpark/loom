use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use loom_config::{AgentConfig, Dirs};
use loom_transport::{AgentEntry, Registry};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::spawn::spawn_agent;
use crate::workspace::{canonical_workspace, resolve_workspace};

pub enum StdioTarget {
    Workspace { path: PathBuf, spawn: bool },
    Instance(String),
}

pub async fn find_target(
    target: &StdioTarget,
    config_path: &Path,
    config: &AgentConfig,
    dirs: &Dirs,
) -> anyhow::Result<AgentEntry> {
    let registry = Registry::new(config.runtime_dir(dirs));
    match target {
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
