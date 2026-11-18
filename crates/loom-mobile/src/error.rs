use loom_client::proto::relay::RelayErrorCode;
use loom_client::proto::{ErrorCode, PROTOCOL_VERSION};
use loom_client::{ClientError, RelayError, agent_protocol, can_update_agent};

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
    /// The relay has no such agent: it stopped, or it was restarted and
    /// runs as another instance now.
    #[error("{message}")]
    Offline { message: String },
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

/// Says which side to update, from the agent's "client speaks protocol 10,
/// agent speaks 11", or the agent's words when they are not in that form.
fn version_mismatch(message: &str) -> String {
    match agent_protocol(message) {
        Some(agent) if agent > PROTOCOL_VERSION => format!(
            "The agent speaks protocol {agent}, newer than this app's {PROTOCOL_VERSION}: \
             update the app."
        ),
        Some(agent) if agent < PROTOCOL_VERSION && can_update_agent(agent) => format!(
            "The agent speaks protocol {agent}, older than this app's {PROTOCOL_VERSION}: \
             use Update agents… in the menu of the host."
        ),
        Some(agent) if agent < PROTOCOL_VERSION => format!(
            "The agent speaks protocol {agent}, older than this app's {PROTOCOL_VERSION}: \
             update loom-agent on the host and restart the agent."
        ),
        _ => format!("{message}; update the app or the agent so that both speak the same version"),
    }
}

impl From<ClientError> for LoomError {
    fn from(err: ClientError) -> Self {
        let message = err.to_string();
        match err {
            ClientError::Rejected(api) if api.code == ErrorCode::VersionMismatch => {
                Self::VersionMismatch {
                    message: version_mismatch(&api.message),
                }
            }
            ClientError::Relay(RelayError::Refused {
                code: RelayErrorCode::NotFound,
                ..
            }) => Self::Offline {
                message: "The agent is not running.".into(),
            },
            ClientError::Rejected(_) => Self::Rejected { message },
            ClientError::Api(api) => Self::Api {
                code: format!("{:?}", api.code),
                message: api.message,
            },
            ClientError::Closed => Self::Closed,
            ClientError::TooLarge { .. } | ClientError::Changed(_) => Self::Invalid { message },
            _ => Self::Connect { message },
        }
    }
}

pub type Result<T, E = LoomError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use loom_client::proto::{ApiError, UPDATE_PROTOCOL_MIN};

    #[test]
    fn a_version_mismatch_says_what_to_update() {
        let err = LoomError::from(ClientError::Rejected(ApiError::new(
            ErrorCode::VersionMismatch,
            format!("client speaks protocol {PROTOCOL_VERSION}, agent speaks 999"),
        )));
        let LoomError::VersionMismatch { message } = err else {
            panic!("{err:?}");
        };
        assert!(message.contains("protocol 999, newer"), "{message}");
        assert!(message.ends_with("update the app."), "{message}");
        let older = format!("client speaks protocol {PROTOCOL_VERSION}, agent speaks 1");
        assert!(version_mismatch(&older).contains("restart the agent"));
        let updatable = format!(
            "client speaks protocol {PROTOCOL_VERSION}, agent speaks {UPDATE_PROTOCOL_MIN}"
        );
        assert!(version_mismatch(&updatable).contains("Update agents…"));
        assert!(version_mismatch("something else").contains("update the app or the agent"));

        let busy = LoomError::from(ClientError::Api(ApiError::new(ErrorCode::Busy, "busy")));
        assert!(matches!(busy, LoomError::Api { code, .. } if code == "Busy"));
        let offline = LoomError::from(ClientError::Relay(RelayError::Refused {
            code: RelayErrorCode::NotFound,
            message: "the agent is offline".into(),
        }));
        assert!(matches!(offline, LoomError::Offline { .. }));
        assert!(matches!(
            LoomError::from(ClientError::Relay(RelayError::Refused {
                code: RelayErrorCode::RateLimited,
                message: "slow down".into(),
            })),
            LoomError::Connect { .. }
        ));
        assert!(matches!(
            LoomError::from(ClientError::Closed),
            LoomError::Closed
        ));
    }
}
