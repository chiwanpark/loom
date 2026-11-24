use loom_proto::ErrorCode;

use crate::error::ClientError;

impl ClientError {
    pub fn agent_protocol(&self) -> Option<u16> {
        match self {
            Self::Rejected(err) if err.code == ErrorCode::VersionMismatch => {
                agent_protocol(&err.message)
            }
            _ => None,
        }
    }
}

pub fn agent_protocol(message: &str) -> Option<u16> {
    message
        .rsplit_once("agent speaks ")
        .and_then(|(_, rest)| rest.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use loom_proto::ApiError;

    use super::*;

    #[test]
    fn reads_the_agent_protocol_from_a_mismatch() {
        let err = ClientError::Rejected(ApiError::new(
            ErrorCode::VersionMismatch,
            "client speaks protocol 19, agent speaks 18",
        ));
        assert_eq!(err.agent_protocol(), Some(18));
        let other = ClientError::Rejected(ApiError::new(ErrorCode::Protocol, "agent speaks 18"));
        assert_eq!(other.agent_protocol(), None);
        assert_eq!(agent_protocol("something else"), None);
    }
}
