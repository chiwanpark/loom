#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("key file is malformed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("the agent rejected the connection: {0}")]
    Rejected(String),
    #[error("the host key does not match the pinned key")]
    HostKeyMismatch,
    #[error("the channel closed during the handshake")]
    Closed,
    #[error("frame of {0} bytes is too large")]
    TooLarge(usize),
}
