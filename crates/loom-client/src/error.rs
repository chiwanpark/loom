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
    #[error("{}", .0.message)]
    Oracle(loom_oracle_proto::OracleError),
    #[error("the connection is closed")]
    Closed,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("{0} changed while it was being read; try again")]
    Changed(String),
    #[error("{path} is {} MB, more than the {} MB that can be opened", .size.div_ceil(1 << 20), .max >> 20)]
    TooLarge { path: String, size: u64, max: u64 },
    #[error("the forwarded connection failed on the agent's side: {0}")]
    Tunnel(String),
    #[error("no agent is running for {0}")]
    NoAgent(String),
    #[error(transparent)]
    Relay(#[from] loom_transport::RelayError),
    #[error(transparent)]
    Crypto(#[from] loom_crypto::CryptoError),
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
