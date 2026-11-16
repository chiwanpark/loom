use std::path::PathBuf;

use async_trait::async_trait;
use loom_proto::{AgentInfo, AgentUpdate, ApiError, ErrorCode, InstanceId};

#[async_trait]
pub trait Supervisor: Send + Sync {
    async fn list(&self) -> Result<Vec<AgentInfo>, ApiError>;
    async fn spawn(&self, workspace: PathBuf) -> Result<AgentInfo, ApiError>;
    async fn stop(&self, instance: &InstanceId) -> Result<(), ApiError>;

    /// Installs a release of `loom-agent`, `version` or the latest, in
    /// place of the binary that this agent runs, and tells which agents
    /// [`Supervisor::restart`] restarts.
    async fn update(&self, _version: Option<String>) -> Result<AgentUpdate, ApiError> {
        Err(ApiError::new(
            ErrorCode::Unsupported,
            "this agent cannot update itself",
        ))
    }

    /// Restarts the agents of `update` with the new binary, this one too.
    async fn restart(&self, _update: &AgentUpdate) {}
}
