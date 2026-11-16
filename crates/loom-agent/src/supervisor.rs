use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use loom_agent_core::{Agent, Supervisor};
use loom_config::Dirs;
use loom_proto::{AgentInfo, AgentUpdate, ApiError, ErrorCode, InstanceId};
use loom_transport::{AgentEntry, Registry};
use loom_update::{Package, UpdateError, Updater, Version};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio_util::sync::CancellationToken;

use crate::spawn::spawn_agent;
use crate::workspace::{WorkspaceError, resolve_workspace};

const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// The signal that makes an agent stop and start again with the binary at
/// its path, as after an update.
pub const RESTART_SIGNAL: Signal = Signal::SIGUSR1;

pub struct LocalSupervisor {
    pub agent: Agent,
    pub registry: Registry,
    pub exe: PathBuf,
    pub config_path: PathBuf,
    pub dirs: Dirs,
    pub instance: InstanceId,
    pub shutdown: CancellationToken,
    /// Set when the agent stops to start again.
    pub restart: Arc<AtomicBool>,
    /// Lets one update run at a time.
    pub updating: tokio::sync::Mutex<()>,
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

fn update_error(err: UpdateError) -> ApiError {
    let code = match &err {
        UpdateError::Unsupported(_) | UpdateError::NotWritable { .. } => ErrorCode::Unsupported,
        UpdateError::Release(_) => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    };
    ApiError::new(code, err.to_string())
}

/// The binary that the process `pid` runs, also after it was replaced.
fn exe_of(pid: u32) -> Option<PathBuf> {
    let link = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let text = link.to_str()?;
    Some(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(text),
    ))
}

impl LocalSupervisor {
    /// The agents that run the binary at `path`: those of this build, which
    /// restart on [`RESTART_SIGNAL`], and the others.
    fn running_from(&self, path: &Path) -> Result<(Vec<AgentInfo>, Vec<AgentInfo>), ApiError> {
        let mut same = Vec::new();
        let mut other = Vec::new();
        for entry in self.registry.list().map_err(internal)? {
            let exe = if entry.instance == self.instance.0 {
                Some(self.exe.clone())
            } else {
                exe_of(entry.pid)
            };
            if exe.as_deref() != Some(path) {
                continue;
            }
            if entry.version == loom_version::VERSION {
                same.push(agent_info(entry));
            } else {
                other.push(agent_info(entry));
            }
        }
        Ok((same, other))
    }
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

    async fn update(&self, version: Option<String>) -> Result<AgentUpdate, ApiError> {
        let version = version
            .as_deref()
            .map(Version::parse)
            .transpose()
            .map_err(|err| ApiError::new(ErrorCode::BadRequest, format!("bad version: {err}")))?;
        let Ok(_guard) = self.updating.try_lock() else {
            return Err(ApiError::new(
                ErrorCode::Busy,
                "the agent is already updating",
            ));
        };
        if self.restart.load(Ordering::SeqCst) {
            return Err(ApiError::new(ErrorCode::Busy, "the agent is restarting"));
        }
        let dir = self
            .exe
            .parent()
            .ok_or_else(|| internal(format!("{} has no directory", self.exe.display())))?;
        let updater = Updater::new();
        let release = updater
            .release(Package::Agent, version.as_ref())
            .await
            .map_err(update_error)?;
        let installed = updater
            .install(Package::Agent, &release, dir, |_, _| {})
            .await
            .map_err(update_error)?;
        let binary = installed
            .into_iter()
            .find(|item| item.binary == "loom-agent")
            .ok_or_else(|| internal("the release has no loom-agent"))?;
        tracing::info!(
            path = %binary.path.display(),
            version = %binary.new,
            "installed a release of loom-agent"
        );
        let (restarting, not_restarted) = self.running_from(&binary.path)?;
        Ok(AgentUpdate {
            previous: binary.old.unwrap_or_default(),
            version: binary.new,
            path: binary.path.display().to_string(),
            restarting,
            not_restarted,
        })
    }

    async fn restart(&self, update: &AgentUpdate) {
        for agent in &update.restarting {
            if agent.instance == self.instance {
                continue;
            }
            let Ok(pid) = i32::try_from(agent.pid) else {
                continue;
            };
            if let Err(err) = kill(Pid::from_raw(pid), RESTART_SIGNAL) {
                tracing::warn!(instance = %agent.instance, error = %err, "could not restart an agent");
            }
        }
        if update
            .restarting
            .iter()
            .any(|a| a.instance == self.instance)
        {
            self.restart.store(true, Ordering::SeqCst);
            self.shutdown.cancel();
        }
    }
}
