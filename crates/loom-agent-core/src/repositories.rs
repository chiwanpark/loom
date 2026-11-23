use async_trait::async_trait;
use loom_proto::{ApiError, PullRequestResult, PushRepositories, RepositoryStatus};

#[async_trait]
pub trait Repositories: Send + Sync {
    async fn list(&self) -> Result<Vec<RepositoryStatus>, ApiError>;
    async fn push(&self, request: PushRepositories) -> Result<Vec<RepositoryStatus>, ApiError>;
    async fn open_pull_requests(
        &self,
        title: String,
        body: String,
        draft: bool,
    ) -> Result<Vec<PullRequestResult>, ApiError>;
}
