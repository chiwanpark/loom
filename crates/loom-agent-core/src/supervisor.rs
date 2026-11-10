use std::path::PathBuf;

use async_trait::async_trait;
use loom_proto::{AgentInfo, ApiError, InstanceId};

#[async_trait]
pub trait Supervisor: Send + Sync {
    async fn list(&self) -> Result<Vec<AgentInfo>, ApiError>;
    async fn spawn(&self, workspace: PathBuf) -> Result<AgentInfo, ApiError>;
    async fn stop(&self, instance: &InstanceId) -> Result<(), ApiError>;
}
