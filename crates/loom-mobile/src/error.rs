use loom_client::ClientError;
use loom_client::proto::ErrorCode;

/// Why a call into the core failed. The message is for the user.
///
/// Kotlin gets a flat error: an exception class per variant whose message is
/// this error's text. (With fields, UniFFI's Kotlin would declare a `message`
/// field next to `Exception.message`, which does not compile.)
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum LoomError {
    /// A bad pairing link, an unknown host, or another bad argument.
    #[error("{message}")]
    Invalid { message: String },
    /// The relay or the agent could not be reached, or the link broke.
    #[error("{message}")]
    Connect { message: String },
    /// The agent speaks another protocol version; one of the two needs an
    /// update.
    #[error("{message}")]
    VersionMismatch { message: String },
    /// The agent turned the connection down, for example because this
    /// device is not paired with it.
    #[error("{message}")]
    Rejected { message: String },
    /// The agent refused a request; `code` is its error code, such as
    /// `Busy` or `NotFound`.
    #[error("{message}")]
    Api { code: String, message: String },
    /// The connection is closed; reconnect to go on.
    #[error("the connection to the agent is closed")]
    Closed,
    /// The app's files could not be read or written.
    #[error("{message}")]
    Storage { message: String },
}

impl LoomError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    pub fn storage(message: impl std::fmt::Display) -> Self {
        Self::Storage {
            message: message.to_string(),
        }
    }
}

impl From<ClientError> for LoomError {
    fn from(err: ClientError) -> Self {
        let message = err.to_string();
        match err {
            ClientError::Rejected(api) if api.code == ErrorCode::VersionMismatch => {
                Self::VersionMismatch {
                    message: format!(
                        "{}; update the app or the agent so that both speak the same version",
                        api.message
                    ),
                }
            }
            ClientError::Rejected(_) => Self::Rejected { message },
            ClientError::Api(api) => Self::Api {
                code: format!("{:?}", api.code),
                message: api.message,
            },
            ClientError::Closed => Self::Closed,
            _ => Self::Connect { message },
        }
    }
}

pub type Result<T, E = LoomError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use loom_client::proto::ApiError;

    #[test]
    fn a_version_mismatch_says_what_to_update() {
        let err = LoomError::from(ClientError::Rejected(ApiError::new(
            ErrorCode::VersionMismatch,
            "the agent speaks protocol 9, the client 8",
        )));
        let LoomError::VersionMismatch { message } = err else {
            panic!("{err:?}");
        };
        assert!(message.starts_with("the agent speaks protocol 9"));
        assert!(message.contains("update the app or the agent"));
        let busy = LoomError::from(ClientError::Api(ApiError::new(ErrorCode::Busy, "busy")));
        assert!(matches!(busy, LoomError::Api { code, .. } if code == "Busy"));
        assert!(matches!(
            LoomError::from(ClientError::Closed),
            LoomError::Closed
        ));
    }
}
