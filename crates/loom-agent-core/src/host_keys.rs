use async_trait::async_trait;
use loom_proto::{ApiError, AuthorizedClient, ErrorCode, HostKeyInfo};

#[async_trait]
pub trait HostKeys: Send + Sync {
    async fn host_key(&self) -> Result<HostKeyInfo, ApiError>;
    async fn authorize(&self, pubkey: [u8; 32], label: String) -> Result<HostKeyInfo, ApiError>;
    async fn list(&self) -> Result<Vec<AuthorizedClient>, ApiError>;
    async fn revoke(&self, label: String) -> Result<(), ApiError>;

    /// Sets or clears the push token of the authorized client with `pubkey`.
    async fn set_push_token(
        &self,
        _pubkey: [u8; 32],
        _token: Option<String>,
    ) -> Result<(), ApiError> {
        Err(ApiError::new(
            ErrorCode::Unsupported,
            "this agent does not send notifications",
        ))
    }
}
