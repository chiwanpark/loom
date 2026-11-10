use std::path::{Path, PathBuf};

use loom_config::{AgentConfig, Dirs};
use loom_transport::{AgentEntry, FrameStream, Registry, SshTarget, connect_command, connect_unix};
use tokio::process::Command;

use crate::error::{ClientError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTarget {
    Workspace { path: PathBuf, spawn: bool },
    Instance(String),
}

impl AgentTarget {
    pub fn stdio_args(&self) -> Vec<String> {
        match self {
            Self::Workspace { path, spawn } => {
                let mut args = vec![
                    "stdio".to_owned(),
                    "--workspace".to_owned(),
                    path.to_string_lossy().into_owned(),
                ];
                if *spawn {
                    args.push("--spawn".to_owned());
                }
                args
            }
            Self::Instance(id) => vec!["stdio".to_owned(), "--instance".to_owned(), id.clone()],
        }
    }
}

pub fn local_registry(dirs: &Dirs, config: &AgentConfig) -> Registry {
    Registry::new(config.runtime_dir(dirs))
}

pub fn list_local_agents(registry: &Registry) -> Result<Vec<AgentEntry>> {
    Ok(registry.list()?)
}

fn find_local(registry: &Registry, target: &AgentTarget) -> Result<Option<AgentEntry>> {
    Ok(match target {
        AgentTarget::Workspace { path, .. } => {
            let path = std::fs::canonicalize(path)?;
            registry.find_workspace(&path)?
        }
        AgentTarget::Instance(id) => registry.find_instance(id)?,
    })
}

pub async fn connect_local(
    registry: &Registry,
    target: &AgentTarget,
    agent_bin: &Path,
) -> Result<FrameStream> {
    if let Some(entry) = find_local(registry, target)? {
        return Ok(connect_unix(&entry.socket).await?);
    }
    match target {
        AgentTarget::Workspace { spawn: true, .. } => {
            let mut command = Command::new(agent_bin);
            command.args(target.stdio_args());
            Ok(connect_command(command)?)
        }
        AgentTarget::Workspace { path, .. } => {
            Err(ClientError::NoAgent(path.display().to_string()))
        }
        AgentTarget::Instance(id) => Err(ClientError::NoAgent(format!("instance {id}"))),
    }
}

pub fn connect_ssh(ssh: &SshTarget, target: &AgentTarget) -> Result<FrameStream> {
    Ok(connect_command(ssh.command(&target.stdio_args()))?)
}

pub async fn list_remote_agents(ssh: &SshTarget) -> Result<Vec<AgentEntry>> {
    let output = ssh
        .command(&["ps".to_owned(), "--json".to_owned()])
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(ClientError::Connect {
            message: format!("`{} ps --json` failed ({})", ssh.remote_bin, output.status),
            diagnostics: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| ClientError::Protocol(format!("invalid agent list: {e}")))
}
