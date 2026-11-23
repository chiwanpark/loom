use serde::Serialize;
use serde::de::DeserializeOwned;

pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(#[from] postcard::Error),
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, CodecError> {
    let bytes = postcard::to_allocvec(message)?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(CodecError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, CodecError> {
    if frame.len() > MAX_FRAME_LEN {
        return Err(CodecError::TooLarge(frame.len()));
    }
    Ok(postcard::from_bytes(frame)?)
}

const REQUEST_VARIANT: u32 = 1;

pub fn undecodable_request_id(frame: &[u8]) -> Option<crate::RequestId> {
    let (variant, id): (u32, crate::RequestId) = postcard::take_from_bytes(frame).ok()?.0;
    (variant == REQUEST_VARIANT).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ClientMessage, Command, Payload, PullRequestResult, PushRepositories, RepositoryStatus,
        ServerMessage,
    };

    #[test]
    fn finds_the_id_of_a_request_with_an_unknown_command() {
        let known = encode(&ClientMessage::Request {
            id: 41,
            command: Command::GetAgentInfo,
        })
        .unwrap();
        assert_eq!(undecodable_request_id(&known), Some(41));
        let mut unknown = postcard::to_allocvec(&(REQUEST_VARIANT, 42u64)).unwrap();
        unknown.extend(postcard::to_allocvec(&9999u32).unwrap());
        assert!(decode::<ClientMessage>(&unknown).is_err());
        assert_eq!(undecodable_request_id(&unknown), Some(42));
        let hello = postcard::to_allocvec(&(0u32, 1u16)).unwrap();
        assert_eq!(undecodable_request_id(&hello), None);
    }

    #[test]
    fn repository_messages_survive_the_codec() {
        for command in [
            Command::ListRepositories,
            Command::PushRepositories(PushRepositories::default()),
            Command::OpenPullRequests {
                title: "t".into(),
                body: String::new(),
                draft: true,
            },
        ] {
            let message = ClientMessage::Request { id: 7, command };
            let back: ClientMessage = decode(&encode(&message).unwrap()).unwrap();
            assert_eq!(back, message);
        }
        let status = RepositoryStatus {
            directory: "app".into(),
            path: "/workspaces/app".into(),
            url: "https://github.com/acme/app.git".into(),
            provider: None,
            branch: "loom/c1".into(),
            base_branch: None,
            changed_files: 0,
            ahead: 0,
            unpushed: 0,
            pushed: false,
            pull_request: None,
            error: None,
        };
        let result = PullRequestResult {
            directory: "app".into(),
            url: None,
            created: false,
            skipped: Some("no changes".into()),
            error: None,
        };
        for payload in [
            Payload::Repositories(vec![status]),
            Payload::PullRequests(vec![result]),
        ] {
            let message = ServerMessage::Response {
                id: 7,
                result: Ok(payload),
            };
            let back: ServerMessage = decode(&encode(&message).unwrap()).unwrap();
            assert_eq!(back, message);
        }
    }
}
