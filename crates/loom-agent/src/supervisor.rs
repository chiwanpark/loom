use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use loom_agent_core::{Agent, Supervisor};
use loom_config::Dirs;
use loom_proto::{AgentInfo, ApiError, ErrorCode, InstanceId};
use loom_transport::{AgentEntry, Registry};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio_util::sync::CancellationToken;

use crate::spawn::spawn_agent;
use crate::workspace::{WorkspaceError, resolve_workspace};

const STOP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct LocalSupervisor {
    pub agent: Agent,
    pub registry: Registry,
    pub exe: PathBuf,
    pub config_path: PathBuf,
    pub dirs: Dirs,
    pub instance: InstanceId,
    pub shutdown: CancellationToken,
}

pub fn agent_info(entry: AgentEntry) -> AgentInfo {
    AgentInfo {
        instance: InstanceId(entry.instance),
        workspace: entry.workspace,
        pid: entry.pid,
        started_at: entry.started_at,
        version: entry.version,
        default_model: None,
    }
}

fn internal(err: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::Internal, err.to_string())
}

#[async_trait]
impl Supervisor for LocalSupervisor {
    async fn list(&self) -> Result<Vec<AgentInfo>, ApiError> {
        let entries = self.registry.list().map_err(internal)?;
        Ok(entries.into_iter().map(agent_info).collect())
    }

    async fn spawn(&self, workspace: PathBuf) -> Result<AgentInfo, ApiError> {
        let config = self.agent.config();
        let workspace =
            resolve_workspace(Some(&workspace), &config, &self.dirs).map_err(|err| match err {
                WorkspaceError::NotAllowed(_) => {
                    ApiError::new(ErrorCode::BadRequest, err.to_string())
                }
                _ => ApiError::new(ErrorCode::NotFound, err.to_string()),
            })?;
        if let Some(entry) = self.registry.find_workspace(&workspace).map_err(internal)? {
            return Ok(agent_info(entry));
        }
        let entry = spawn_agent(
            &self.exe,
            &self.config_path,
            &workspace,
            &self.registry,
            &self.dirs,
        )
        .await
        .map_err(internal)?;
        Ok(agent_info(entry))
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), ApiError> {
        if *instance == self.instance {
            self.shutdown.cancel();
            return Ok(());
        }
        let entry = self
            .registry
            .find_instance(&instance.0)
            .map_err(internal)?
            .ok_or_else(|| {
                ApiError::new(
                    ErrorCode::NotFound,
                    format!("no agent with instance {instance}"),
                )
            })?;
        let pid = i32::try_from(entry.pid).map_err(internal)?;
        kill(Pid::from_raw(pid), Signal::SIGTERM).map_err(internal)?;
        if self
            .registry
            .wait_until_gone(&entry.instance, STOP_TIMEOUT)
            .await
            .map_err(internal)?
        {
            Ok(())
        } else {
            Err(internal(format!(
                "{instance} did not stop within {STOP_TIMEOUT:?}"
            )))
        }
    }
}
