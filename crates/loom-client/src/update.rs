use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use loom_proto::{
    AgentUpdate, ClientHello, ClientMessage, Command, ErrorCode, Payload, ServerMessage,
    UPDATE_PROTOCOL_MIN, decode, encode,
};
use loom_transport::FrameStream;

use crate::client::Client;
use crate::error::{ClientError, Result};

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_REQUEST: u64 = 1;

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

pub fn can_update_agent(protocol: u16) -> bool {
    protocol >= UPDATE_PROTOCOL_MIN
}

pub async fn update_agent_with<F, Fut>(
    connect: F,
    client_name: &str,
    version: Option<String>,
) -> Result<AgentUpdate>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<FrameStream>>,
{
    match Client::connect(connect().await?, client_name).await {
        Ok((client, _notifications)) => client.update_agent(version).await,
        Err(err) => match err.agent_protocol() {
            Some(protocol) => {
                update_agent_of_protocol(connect().await?, client_name, protocol, version).await
            }
            None => Err(err),
        },
    }
}

pub async fn update_agent_of_protocol(
    stream: FrameStream,
    client_name: &str,
    protocol: u16,
    version: Option<String>,
) -> Result<AgentUpdate> {
    if !can_update_agent(protocol) {
        return Err(ClientError::Protocol(format!(
            "the agent speaks protocol {protocol}, which cannot update agents; \
             run `loom-agent update` on its host"
        )));
    }
    let (mut sender, mut receiver, _) = stream.split();
    let hello = ClientMessage::Hello(ClientHello {
        version: protocol,
        client: client_name.to_owned(),
    });
    sender.send(Bytes::from(encode(&hello)?)).await?;
    let greeted = async {
        loop {
            let Some(frame) = receiver.recv().await? else {
                return Err(ClientError::Closed);
            };
            match decode::<ServerMessage>(&frame) {
                Ok(ServerMessage::Hello(_)) => return Ok(()),
                Ok(ServerMessage::Rejected(err)) => return Err(ClientError::Rejected(err)),
                _ if frame.first() == Some(&0) => return Ok(()),
                _ => {}
            }
        }
    };
    tokio::time::timeout(HELLO_TIMEOUT, greeted)
        .await
        .map_err(|_| ClientError::Protocol("timed out waiting for the agent".into()))??;
    let request = ClientMessage::Request {
        id: UPDATE_REQUEST,
        command: Command::UpdateAgent { version },
    };
    sender.send(Bytes::from(encode(&request)?)).await?;
    loop {
        let Some(frame) = receiver.recv().await? else {
            return Err(ClientError::Closed);
        };
        match decode::<ServerMessage>(&frame) {
            Ok(ServerMessage::Response { id, result }) if id == UPDATE_REQUEST => {
                return match result {
                    Ok(Payload::AgentUpdated(update)) => Ok(update),
                    Ok(other) => Err(ClientError::Protocol(format!(
                        "unexpected response {other:?}"
                    ))),
                    Err(err) => Err(ClientError::Api(err)),
                };
            }
            Ok(ServerMessage::Rejected(err)) => return Err(ClientError::Rejected(err)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use loom_proto::{AgentInfo, ApiError, InstanceId, ServerHello};
    use loom_transport::memory_pair;

    use super::*;

    fn update() -> AgentUpdate {
        AgentUpdate {
            previous: "loom-agent 0.4.1".into(),
            version: "loom-agent 0.5.0".into(),
            path: "/bin/loom-agent".into(),
            restarting: Vec::new(),
            not_restarted: Vec::new(),
        }
    }

    async fn old_agent(
        mut stream: FrameStream,
        protocol: u16,
        result: std::result::Result<Payload, ApiError>,
    ) -> Option<String> {
        let hello: ClientMessage = decode(&stream.recv().await.unwrap().unwrap()).unwrap();
        assert!(matches!(hello, ClientMessage::Hello(h) if h.version == protocol));
        let agent = AgentInfo {
            instance: InstanceId("i".into()),
            workspace: "/w".into(),
            pid: 1,
            started_at: 0,
            version: "0.4.1".into(),
            default_model: None,
        };
        let hello = ServerMessage::Hello(ServerHello {
            version: protocol,
            agent,
        });
        stream.send(encode(&hello).unwrap().into()).await.unwrap();
        stream.send(Bytes::from_static(&[6, 200, 3])).await.unwrap();
        let request: ClientMessage = decode(&stream.recv().await.unwrap().unwrap()).unwrap();
        let ClientMessage::Request {
            id,
            command: Command::UpdateAgent { version },
        } = request
        else {
            panic!("{request:?}");
        };
        let response = ServerMessage::Response { id, result };
        stream
            .send(encode(&response).unwrap().into())
            .await
            .unwrap();
        version
    }

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

    #[tokio::test]
    async fn updates_an_agent_of_another_protocol() {
        let (client, agent) = memory_pair();
        let agent = tokio::spawn(old_agent(agent, 18, Ok(Payload::AgentUpdated(update()))));
        let result = update_agent_of_protocol(client, "test", 18, Some("0.5.0".into())).await;
        assert_eq!(result.unwrap(), update());
        assert_eq!(agent.await.unwrap().as_deref(), Some("0.5.0"));

        let (client, agent) = memory_pair();
        let failure = ApiError::new(ErrorCode::NotFound, "no such release");
        let agent = tokio::spawn(old_agent(agent, 15, Err(failure.clone())));
        let result = update_agent_of_protocol(client, "test", 15, None).await;
        assert!(matches!(result, Err(ClientError::Api(err)) if err == failure));
        assert_eq!(agent.await.unwrap(), None);
    }

    #[tokio::test]
    async fn updates_through_another_connection_after_a_mismatch() {
        let (first, mut rejecting) = memory_pair();
        let (second, agent) = memory_pair();
        let rejecting = tokio::spawn(async move {
            rejecting.recv().await.unwrap().unwrap();
            let rejected = ServerMessage::Rejected(ApiError::new(
                ErrorCode::VersionMismatch,
                "client speaks protocol 19, agent speaks 17",
            ));
            rejecting
                .send(encode(&rejected).unwrap().into())
                .await
                .unwrap();
        });
        let agent = tokio::spawn(old_agent(agent, 17, Ok(Payload::AgentUpdated(update()))));
        let streams = std::sync::Mutex::new(vec![second, first]);
        let result = update_agent_with(
            || async { Ok(streams.lock().unwrap().pop().unwrap()) },
            "test",
            None,
        )
        .await;
        assert_eq!(result.unwrap(), update());
        rejecting.await.unwrap();
        assert_eq!(agent.await.unwrap(), None);
        assert!(streams.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_agents_older_than_the_update_command() {
        let (client, _agent) = memory_pair();
        let result = update_agent_of_protocol(client, "test", UPDATE_PROTOCOL_MIN - 1, None).await;
        assert!(
            matches!(&result, Err(ClientError::Protocol(m)) if m.contains("loom-agent update")),
            "{result:?}"
        );
    }
}
