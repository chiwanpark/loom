use async_trait::async_trait;
use loom_proto::{ApiError, AuthorizedClient, HostKeyInfo};

#[async_trait]
pub trait HostKeys: Send + Sync {
    async fn host_key(&self) -> Result<HostKeyInfo, ApiError>;
    async fn authorize(&self, pubkey: [u8; 32], label: String) -> Result<HostKeyInfo, ApiError>;
    async fn list(&self) -> Result<Vec<AuthorizedClient>, ApiError>;
    async fn revoke(&self, label: String) -> Result<(), ApiError>;
}
