use loom_proto::{ApiError, CodecError};

pub type Result<T, E = ClientError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("{message}\n{diagnostics}")]
    Connect {
        message: String,
        diagnostics: String,
    },
    #[error("the agent rejected the connection: {0}")]
    Rejected(ApiError),
    #[error("{}", .0.message)]
    Api(ApiError),
    #[error("the connection is closed")]
    Closed,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("no agent is running for {0}")]
    NoAgent(String),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ClientError {
    pub fn api(&self) -> Option<&ApiError> {
        match self {
            Self::Api(err) | Self::Rejected(err) => Some(err),
            _ => None,
        }
    }
}
