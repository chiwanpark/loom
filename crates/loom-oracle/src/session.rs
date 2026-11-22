use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use loom_oracle_proto::{
    ClientMessage, Command, DeviceResult, Devices, Empty, ErrorCode, Event, InvalidMessage,
    OracleError, PROTOCOL_VERSION, Pairings, PeerInfo, RelayHostResult, RelayHosts, Request,
    Response, Role, ServerHello, ServerMessage, encode, parse_client_message,
};
use loom_transport::{FrameReceiver, FrameStream};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::oracle::{Oracle, pairing_ttl};
use crate::store::PeerRow;

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const OUT_QUEUE: usize = 256;
const WRITER_DRAIN: Duration = Duration::from_secs(2);

fn ok(value: impl Serialize) -> Result<Value, OracleError> {
    serde_json::to_value(value).map_err(|e| OracleError::new(ErrorCode::Internal, e.to_string()))
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<Event>>,
) -> Result<Event, broadcast::error::RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

enum Step {
    Stop,
    Frame(Bytes),
    Event(Result<Event, broadcast::error::RecvError>),
}

pub struct Session {
    oracle: Arc<Oracle>,
    peer: PeerRow,
    kill: CancellationToken,
    events: Option<broadcast::Receiver<Event>>,
}

impl Session {
    pub fn new(oracle: Arc<Oracle>, peer: PeerRow, kill: CancellationToken) -> Self {
        Self {
            oracle,
            peer,
            kill,
            events: None,
        }
    }

    pub async fn run(mut self, stream: FrameStream) {
        let (mut sender, mut receiver, _) = stream.split();
        let (out, mut queued) = mpsc::channel::<Vec<u8>>(OUT_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(bytes) = queued.recv().await {
                if sender.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
            let _ = sender.close().await;
        });
        if self.handshake(&mut receiver, &out).await {
            self.serve(&mut receiver, &out).await;
        }
        drop(out);
        let _ = tokio::time::timeout(WRITER_DRAIN, writer).await;
    }

    async fn send(&self, out: &mpsc::Sender<Vec<u8>>, message: &ServerMessage) -> bool {
        let Ok(bytes) = encode(message) else {
            return false;
        };
        tokio::select! {
            biased;
            sent = out.send(bytes) => sent.is_ok(),
            _ = self.kill.cancelled() => false,
        }
    }

    async fn handshake(&self, receiver: &mut FrameReceiver, out: &mpsc::Sender<Vec<u8>>) -> bool {
        let frame = tokio::select! {
            _ = self.kill.cancelled() => return false,
            frame = tokio::time::timeout(HELLO_TIMEOUT, receiver.recv()) => frame,
        };
        let Ok(Ok(Some(bytes))) = frame else {
            return false;
        };
        match parse_client_message(&bytes) {
            Ok(ClientMessage::Hello(hello)) => {
                tracing::debug!(
                    peer = %self.peer.label,
                    client = %hello.client,
                    version = hello.version,
                    "control client said hello"
                );
                let reply = ServerMessage::Hello(ServerHello {
                    version: PROTOCOL_VERSION,
                    oracle: self.oracle.info(),
                    peer: PeerInfo {
                        id: self.peer.id,
                        role: self.peer.role,
                        label: self.peer.label.clone(),
                    },
                });
                self.send(out, &reply).await
            }
            _ => {
                let error =
                    OracleError::new(ErrorCode::BadRequest, "the first message must be hello");
                self.send(out, &ServerMessage::Rejected(error)).await;
                false
            }
        }
    }

    async fn serve(&mut self, receiver: &mut FrameReceiver, out: &mpsc::Sender<Vec<u8>>) {
        loop {
            let step = tokio::select! {
                _ = self.kill.cancelled() => Step::Stop,
                frame = receiver.recv() => match frame {
                    Ok(Some(bytes)) => Step::Frame(bytes),
                    _ => Step::Stop,
                },
                event = next_event(&mut self.events) => Step::Event(event),
            };
            let message = match step {
                Step::Stop => break,
                Step::Event(Ok(event)) => ServerMessage::Event { event },
                Step::Event(Err(broadcast::error::RecvError::Lagged(_))) => ServerMessage::Event {
                    event: Event::EventsLost,
                },
                Step::Event(Err(broadcast::error::RecvError::Closed)) => {
                    self.events = None;
                    continue;
                }
                Step::Frame(bytes) => match parse_client_message(&bytes) {
                    Ok(ClientMessage::Request(Request { id, command })) => {
                        ServerMessage::Response(match self.handle(command) {
                            Ok(result) => Response {
                                id,
                                result: Some(result),
                                error: None,
                            },
                            Err(error) => Response::error(id, error),
                        })
                    }
                    Ok(ClientMessage::Hello(_)) => continue,
                    Err(InvalidMessage {
                        id: Some(id),
                        error,
                    }) => ServerMessage::Response(Response::error(id, error)),
                    Err(InvalidMessage { id: None, error }) => {
                        tracing::debug!(peer = %self.peer.label, %error, "ignored a malformed message");
                        continue;
                    }
                },
            };
            if !self.send(out, &message).await {
                break;
            }
        }
    }

    fn handle(&mut self, command: Command) -> Result<Value, OracleError> {
        if command == Command::OracleInfo {
            return ok(self.oracle.info());
        }
        if self.peer.role != Role::Device {
            return Err(OracleError::new(
                ErrorCode::Forbidden,
                format!("a {} cannot do this; only devices can", self.peer.role),
            ));
        }
        let oracle = &self.oracle;
        let me = Some(self.peer.id);
        match command {
            Command::OracleInfo => ok(oracle.info()),
            Command::Subscribe => {
                if self.events.is_none() {
                    self.events = Some(oracle.subscribe());
                }
                ok(Empty {})
            }
            Command::Unsubscribe => {
                self.events = None;
                ok(Empty {})
            }
            Command::ListDevices => ok(Devices {
                devices: oracle.devices()?,
            }),
            Command::RenameDevice { id, label } => ok(DeviceResult {
                device: oracle.rename_device(id, &label, me)?,
            }),
            Command::RevokeDevice { id } => {
                oracle.revoke_device(id, me)?;
                ok(Empty {})
            }
            Command::SetPushToken { token } => {
                oracle.set_push_token(self.peer.id, token.as_deref())?;
                ok(Empty {})
            }
            Command::CreateDevicePairing { ttl_secs } => {
                ok(oracle.create_pairing(Role::Device, pairing_ttl(ttl_secs)?, me)?)
            }
            Command::ListPairings => ok(Pairings {
                pairings: oracle.pairings()?,
            }),
            Command::CancelPairing { id } => {
                oracle.cancel_pairing(&id, me)?;
                ok(Empty {})
            }
            Command::ListRelayHosts => ok(RelayHosts {
                relay_hosts: oracle.relay_hosts()?,
            }),
            Command::AddRelayHost { host_id, label } => ok(RelayHostResult {
                relay_host: oracle.add_relay_host(&host_id, &label, me)?,
            }),
            Command::RemoveRelayHost { host_id } => {
                oracle.remove_relay_host(&host_id, me)?;
                ok(Empty {})
            }
        }
    }
}
