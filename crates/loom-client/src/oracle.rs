use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use loom_crypto::{ClientIdentity, PairingUri, decode_key, encode_key};
use loom_oracle_proto::{
    ClientHello, ClientMessage, Command, Device, DeviceResult, Devices, Empty, Event,
    ORACLE_INSTANCE, OracleInfo, PROTOCOL_VERSION, Pairing, PairingLink, Pairings, RelayHost,
    RelayHostResult, RelayHosts, Request, ServerHello, ServerMessage, encode, parse_server_message,
};
use loom_proto::InstanceId;
use loom_proto::relay::{AgentId, HostId};
use loom_transport::{AccessToken, FrameReceiver, FrameSender, FrameStream, RelayEndpoint};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::error::{ClientError, Result};

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_QUEUE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleEndpoint {
    pub url: String,
    pub host: HostId,
    pub host_key: [u8; 32],
    pub access: Option<AccessToken>,
}

impl OracleEndpoint {
    pub fn from_parts(
        url: &str,
        host_id: &str,
        host_key: &str,
        access: Option<AccessToken>,
    ) -> Result<Self> {
        let host = host_id
            .parse()
            .map_err(|_| ClientError::Protocol(format!("invalid host ID {host_id:?}")))?;
        let host_key =
            decode_key(host_key).ok_or_else(|| ClientError::Protocol("invalid host key".into()))?;
        Ok(Self {
            url: url.to_owned(),
            host,
            host_key,
            access,
        })
    }

    pub fn host_key_text(&self) -> String {
        encode_key(&self.host_key)
    }

    fn endpoint(&self) -> RelayEndpoint {
        RelayEndpoint {
            url: self.url.clone(),
            access: self.access.clone(),
        }
    }

    pub fn agent(&self) -> AgentId {
        AgentId {
            host: self.host,
            instance: InstanceId(ORACLE_INSTANCE.into()),
        }
    }

    pub async fn connect_stream(&self, identity: &ClientIdentity) -> Result<FrameStream> {
        Ok(
            loom_transport::connect_relay(&self.endpoint(), self.agent(), identity, &self.host_key)
                .await?,
        )
    }
}

pub async fn pair_oracle(
    link: &str,
    identity: &ClientIdentity,
    label: &str,
    access: Option<AccessToken>,
) -> Result<OracleEndpoint> {
    let uri: PairingUri = link.trim().parse()?;
    if uri.instance.0 != ORACLE_INSTANCE {
        return Err(ClientError::Protocol(
            "this link pairs with an agent, not with an oracle".into(),
        ));
    }
    let endpoint = OracleEndpoint {
        url: uri.relay_url.clone(),
        host: uri.host,
        host_key: uri.host_static,
        access,
    };
    loom_transport::pair_relay(&endpoint.endpoint(), &uri, identity, label).await?;
    Ok(endpoint)
}

type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value>>>>>>;

#[derive(Clone)]
pub struct OracleClient {
    hello: Arc<ServerHello>,
    writer: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

pub struct OracleEvents {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl OracleEvents {
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

fn protocol(err: serde_json::Error) -> ClientError {
    ClientError::Protocol(err.to_string())
}

async fn send_message(sender: &mut FrameSender, message: &ClientMessage) -> Result<()> {
    sender
        .send(Bytes::from(encode(message).map_err(protocol)?))
        .await?;
    Ok(())
}

async fn recv_message(receiver: &mut FrameReceiver) -> Result<Option<ServerMessage>> {
    loop {
        let Some(frame) = receiver.recv().await? else {
            return Ok(None);
        };
        if let Some(message) = parse_server_message(&frame).map_err(protocol)? {
            return Ok(Some(message));
        }
    }
}

impl OracleClient {
    pub async fn connect(
        endpoint: &OracleEndpoint,
        identity: &ClientIdentity,
        client_name: &str,
    ) -> Result<(Self, OracleEvents)> {
        let stream = endpoint.connect_stream(identity).await?;
        Self::over(stream, client_name).await
    }

    pub async fn over(stream: FrameStream, client_name: &str) -> Result<(Self, OracleEvents)> {
        let (mut sender, mut receiver, _) = stream.split();
        let handshake = async {
            send_message(
                &mut sender,
                &ClientMessage::Hello(ClientHello {
                    version: PROTOCOL_VERSION,
                    client: client_name.to_owned(),
                }),
            )
            .await?;
            loop {
                match recv_message(&mut receiver).await? {
                    Some(ServerMessage::Hello(hello)) => return Ok(hello),
                    Some(ServerMessage::Rejected(err)) => return Err(ClientError::Oracle(err)),
                    Some(_) => continue,
                    None => return Err(ClientError::Closed),
                }
            }
        };
        let hello = tokio::time::timeout(HELLO_TIMEOUT, handshake)
            .await
            .map_err(|_| ClientError::Protocol("the oracle did not say hello in time".into()))??;

        let (writer, mut queued) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
        tokio::spawn(async move {
            while let Some(bytes) = queued.recv().await {
                if sender.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
            let _ = sender.close().await;
        });
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events, rx) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            loop {
                match recv_message(&mut receiver).await {
                    Ok(Some(ServerMessage::Response(response))) => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_mut()
                            .and_then(|pending| pending.remove(&response.id));
                        if let Some(waiter) = waiter {
                            let result = match response.error {
                                Some(err) => Err(ClientError::Oracle(err)),
                                None => Ok(response
                                    .result
                                    .unwrap_or_else(|| Value::Object(Default::default()))),
                            };
                            let _ = waiter.send(result);
                        }
                    }
                    Ok(Some(ServerMessage::Event { event })) => {
                        let _ = events.send(event);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(error = %err, "the oracle connection failed");
                        break;
                    }
                }
            }
            reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        });
        Ok((
            Self {
                hello: Arc::new(hello),
                writer,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            OracleEvents { rx },
        ))
    }

    pub fn hello(&self) -> &ServerHello {
        &self.hello
    }

    pub fn is_closed(&self) -> bool {
        self.writer.is_closed()
            || self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
    }

    pub async fn call<T: DeserializeOwned>(&self, command: Command) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let Some(pending) = pending.as_mut() else {
                return Err(ClientError::Closed);
            };
            pending.insert(id, tx);
        }
        let bytes = encode(&ClientMessage::Request(Request { id, command })).map_err(protocol)?;
        if self.writer.send(bytes).await.is_err() {
            return Err(ClientError::Closed);
        }
        let value = rx.await.map_err(|_| ClientError::Closed)??;
        serde_json::from_value(value).map_err(protocol)
    }

    pub async fn info(&self) -> Result<OracleInfo> {
        self.call(Command::OracleInfo).await
    }

    pub async fn subscribe(&self) -> Result<()> {
        self.call::<Empty>(Command::Subscribe).await.map(|_| ())
    }

    pub async fn unsubscribe(&self) -> Result<()> {
        self.call::<Empty>(Command::Unsubscribe).await.map(|_| ())
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        Ok(self.call::<Devices>(Command::ListDevices).await?.devices)
    }

    pub async fn rename_device(&self, id: u64, label: &str) -> Result<Device> {
        let label = label.to_owned();
        Ok(self
            .call::<DeviceResult>(Command::RenameDevice { id, label })
            .await?
            .device)
    }

    pub async fn revoke_device(&self, id: u64) -> Result<()> {
        self.call::<Empty>(Command::RevokeDevice { id })
            .await
            .map(|_| ())
    }

    pub async fn set_push_token(&self, token: Option<&str>) -> Result<()> {
        let token = token.map(str::to_owned);
        self.call::<Empty>(Command::SetPushToken { token })
            .await
            .map(|_| ())
    }

    pub async fn create_device_pairing(&self, ttl: Option<Duration>) -> Result<PairingLink> {
        let ttl_secs = ttl.map(|ttl| ttl.as_secs());
        self.call(Command::CreateDevicePairing { ttl_secs }).await
    }

    pub async fn pairings(&self) -> Result<Vec<Pairing>> {
        Ok(self.call::<Pairings>(Command::ListPairings).await?.pairings)
    }

    pub async fn cancel_pairing(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.call::<Empty>(Command::CancelPairing { id })
            .await
            .map(|_| ())
    }

    pub async fn relay_hosts(&self) -> Result<Vec<RelayHost>> {
        Ok(self
            .call::<RelayHosts>(Command::ListRelayHosts)
            .await?
            .relay_hosts)
    }

    pub async fn add_relay_host(&self, host_id: &str, label: &str) -> Result<RelayHost> {
        let (host_id, label) = (host_id.to_owned(), label.to_owned());
        Ok(self
            .call::<RelayHostResult>(Command::AddRelayHost { host_id, label })
            .await?
            .relay_host)
    }

    pub async fn remove_relay_host(&self, host_id: &str) -> Result<()> {
        let host_id = host_id.to_owned();
        self.call::<Empty>(Command::RemoveRelayHost { host_id })
            .await
            .map(|_| ())
    }
}
