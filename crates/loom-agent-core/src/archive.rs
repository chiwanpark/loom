use async_trait::async_trait;
use loom_proto::{ApiError, ArchivedSessionInfo, SessionId};

use crate::store::StoredSession;

#[async_trait]
pub trait SessionArchive: Send + Sync {
    async fn list(&self) -> Result<Vec<ArchivedSessionInfo>, ApiError>;
    async fn fetch(&self, codespace: &str, session: &SessionId) -> Result<StoredSession, ApiError>;
    async fn resumed(&self, codespace: &str, session: &SessionId) -> Result<(), ApiError>;
}
