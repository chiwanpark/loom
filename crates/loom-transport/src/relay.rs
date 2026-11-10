use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use loom_crypto::{
    ClientIdentity, CryptoError, HostIdentity, Mode, PacketChannel, PairingUri, Peer, Responder,
};
use loom_proto::InstanceId;
use loom_proto::relay::{
    AgentId, ChannelId, CloseReason, HostId, RELAY_VERSION, RelayErrorCode, RelayFrame,
    agent_auth_message,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::frame::{FrameStream, PeerInfo};
use crate::ws::{self, AbortOnDrop, RelayEndpoint, WsLink};

pub const AGENT_PATH: &str = "/v1/agent";
pub const CLIENT_PATH: &str = "/v1/client";
const CHANNEL_QUEUE: usize = 1024;
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("cannot reach the relay: {0}")]
    Connect(String),
    #[error("the relay refused: {message}")]
    Refused {
        code: RelayErrorCode,
        message: String,
    },
    #[error("the relay closed the channel ({0:?})")]
    ChannelClosed(CloseReason),
    #[error("the relay connection closed")]
    Closed,
    #[error("the relay did not answer in time")]
    Timeout,
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

async fn next(link: &mut WsLink) -> Result<RelayFrame, RelayError> {
    match tokio::time::timeout(REPLY_TIMEOUT, link.rx.recv()).await {
        Ok(Some(frame)) => Ok(frame),
        Ok(None) => Err(RelayError::Closed),
        Err(_) => Err(RelayError::Timeout),
    }
}

async fn send(link: &WsLink, frame: RelayFrame) -> Result<(), RelayError> {
    link.tx.send(frame).await.map_err(|_| RelayError::Closed)
}

pub struct RelayClient {
    link: WsLink,
}

impl RelayClient {
    pub async fn connect(endpoint: &RelayEndpoint) -> Result<Self, RelayError> {
        Ok(Self {
            link: ws::connect(endpoint, CLIENT_PATH).await?,
        })
    }

    pub async fn list_agents(&mut self, host: HostId) -> Result<Vec<InstanceId>, RelayError> {
        send(&self.link, RelayFrame::ListAgents { host }).await?;
        loop {
            match next(&mut self.link).await? {
                RelayFrame::Agents { host: h, instances } if h == host => return Ok(instances),
                RelayFrame::Error { code, message } => {
                    return Err(RelayError::Refused { code, message });
                }
                _ => {}
            }
        }
    }

    pub async fn open(
        mut self,
        agent: AgentId,
    ) -> Result<(PacketChannel, AbortOnDrop), RelayError> {
        send(&self.link, RelayFrame::Open { agent }).await?;
        let channel = loop {
            match next(&mut self.link).await? {
                RelayFrame::Opened { channel, .. } => break channel,
                RelayFrame::Error { code, message } => {
                    return Err(RelayError::Refused { code, message });
                }
                _ => {}
            }
        };
        let (mut ours, theirs) = PacketChannel::pair(CHANNEL_QUEUE);
        let link = self.link;
        let task = tokio::spawn(async move {
            let mut link = link;
            loop {
                tokio::select! {
                    packet = ours.rx.recv() => match packet {
                        Some(payload) => {
                            if link.tx.send(RelayFrame::Data { channel, payload }).await.is_err() {
                                break;
                            }
                        }
                        None => {
                            let _ = link.tx.send(RelayFrame::Close { channel }).await;
                            break;
                        }
                    },
                    frame = link.rx.recv() => match frame {
                        Some(RelayFrame::Data { channel: c, payload }) if c == channel => {
                            if ours.tx.send(payload).await.is_err() {
                                break;
                            }
                        }
                        Some(RelayFrame::Closed { channel: c, reason }) if c == channel => {
                            tracing::debug!(channel, ?reason, "the relay closed the channel");
                            break;
                        }
                        Some(other) => tracing::debug!(?other, "ignored a relay frame"),
                        None => {
                            tracing::debug!(channel, "the relay connection ended");
                            break;
                        }
                    },
                }
            }
        });
        Ok((theirs, AbortOnDrop(task)))
    }
}

pub async fn connect_relay(
    endpoint: &RelayEndpoint,
    agent: AgentId,
    identity: &ClientIdentity,
    host_static: &[u8; 32],
) -> Result<FrameStream, RelayError> {
    let label = agent.instance.0.clone();
    let client = RelayClient::connect(endpoint).await?;
    let (channel, guard) = client.open(agent).await?;
    let secure = loom_crypto::initiate(channel, identity, host_static, Mode::Connect).await?;
    Ok(FrameStream::secure(
        secure,
        PeerInfo::Relay {
            label,
            pubkey: *host_static,
        },
        Some(Arc::new(guard)),
    ))
}

pub async fn pair_relay(
    endpoint: &RelayEndpoint,
    uri: &PairingUri,
    identity: &ClientIdentity,
    label: &str,
) -> Result<(), RelayError> {
    let client = RelayClient::connect(endpoint).await?;
    let (channel, _guard) = client
        .open(AgentId {
            host: uri.host,
            instance: uri.instance.clone(),
        })
        .await?;
    loom_crypto::initiate(
        channel,
        identity,
        &uri.host_static,
        Mode::Pair {
            secret: &uri.secret,
            label,
        },
    )
    .await?;
    Ok(())
}

pub struct AgentLinkOptions {
    pub endpoint: RelayEndpoint,
    pub instance: InstanceId,
    pub host: Arc<HostIdentity>,
    pub responder: Arc<dyn Responder>,
}

pub struct IncomingChannel {
    pub channel: ChannelId,
    pub peer: Peer,
    pub stream: FrameStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    Connecting,
    Registered,
    Disconnected(String),
}

pub fn spawn_agent_link(
    options: AgentLinkOptions,
    incoming: mpsc::Sender<IncomingChannel>,
    shutdown: CancellationToken,
) -> (JoinHandle<()>, watch::Receiver<LinkState>) {
    let (state_tx, state_rx) = watch::channel(LinkState::Connecting);
    let options = Arc::new(options);
    let task = tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            let _ = state_tx.send(LinkState::Connecting);
            let mut registered = false;
            let result = run_link(&options, &incoming, &state_tx, &shutdown, &mut registered).await;
            match result {
                Ok(()) => break,
                Err(err) => {
                    tracing::warn!(error = %err, "relay connection lost");
                    let _ = state_tx.send(LinkState::Disconnected(err.to_string()));
                }
            }
            if registered {
                backoff = Duration::from_secs(1);
            }
            let jitter = Duration::from_millis(rand::random_range(0..500));
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(backoff + jitter) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    });
    (task, state_rx)
}

async fn run_link(
    options: &Arc<AgentLinkOptions>,
    incoming: &mpsc::Sender<IncomingChannel>,
    state: &watch::Sender<LinkState>,
    shutdown: &CancellationToken,
    registered: &mut bool,
) -> Result<(), RelayError> {
    let mut link = ws::connect(&options.endpoint, AGENT_PATH).await?;
    send(
        &link,
        RelayFrame::AgentHello {
            host_pubkey: options.host.verifying_key(),
            instance: options.instance.clone(),
            version: RELAY_VERSION,
        },
    )
    .await?;
    loop {
        match next(&mut link).await? {
            RelayFrame::Challenge { nonce } => {
                let signature = options
                    .host
                    .sign(&agent_auth_message(&nonce, &options.instance));
                send(&link, RelayFrame::AgentAuth { signature }).await?;
            }
            RelayFrame::Registered { .. } => break,
            RelayFrame::Error { code, message } => {
                return Err(RelayError::Refused { code, message });
            }
            _ => {}
        }
    }
    *registered = true;
    let _ = state.send(LinkState::Registered);
    tracing::info!(relay = %options.endpoint.url, "registered with the relay");

    let mut channels: HashMap<ChannelId, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let (out_tx, mut out_rx) = mpsc::channel::<(ChannelId, Option<Vec<u8>>)>(CHANNEL_QUEUE);
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            frame = link.rx.recv() => {
                let Some(frame) = frame else {
                    return Err(RelayError::Closed);
                };
                match frame {
                    RelayFrame::Opened { channel, .. } => {
                        let (ours, theirs) = PacketChannel::pair(CHANNEL_QUEUE);
                        let PacketChannel { tx: ours_tx, rx: mut ours_rx } = ours;
                        channels.insert(channel, ours_tx);
                        let out = out_tx.clone();
                        tasks.spawn(async move {
                            while let Some(packet) = ours_rx.recv().await {
                                if out.send((channel, Some(packet))).await.is_err() {
                                    return;
                                }
                            }
                            let _ = out.send((channel, None)).await;
                        });
                        let options = options.clone();
                        let incoming = incoming.clone();
                        tasks.spawn(async move {
                            match loom_crypto::respond(theirs, &options.host, options.responder.as_ref()).await {
                                Ok((secure, peer)) => {
                                    let stream = FrameStream::secure(
                                        secure,
                                        PeerInfo::Relay {
                                            label: peer.label.clone(),
                                            pubkey: peer.pubkey,
                                        },
                                        None,
                                    );
                                    let _ = incoming
                                        .send(IncomingChannel { channel, peer, stream })
                                        .await;
                                }
                                Err(err) => {
                                    tracing::info!(channel, error = %err, "relay handshake failed");
                                }
                            }
                        });
                    }
                    RelayFrame::Data { channel, payload } => {
                        let overflow = match channels.get(&channel) {
                            Some(tx) => match tx.try_send(payload) {
                                Ok(()) => false,
                                Err(mpsc::error::TrySendError::Full(_)) => true,
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    channels.remove(&channel);
                                    false
                                }
                            },
                            None => false,
                        };
                        if overflow {
                            tracing::warn!(channel, "closing a relay channel that fell behind");
                            channels.remove(&channel);
                            send(&link, RelayFrame::Close { channel }).await?;
                        }
                    }
                    RelayFrame::Closed { channel, .. } => {
                        channels.remove(&channel);
                    }
                    RelayFrame::Error { code, message } => {
                        tracing::warn!(?code, %message, "relay error");
                    }
                    _ => {}
                }
            }
            item = out_rx.recv() => {
                let Some((channel, packet)) = item else { continue };
                match packet {
                    Some(payload) => {
                        if channels.contains_key(&channel) {
                            send(&link, RelayFrame::Data { channel, payload }).await?;
                        }
                    }
                    None => {
                        if channels.remove(&channel).is_some() {
                            send(&link, RelayFrame::Close { channel }).await?;
                        }
                    }
                }
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
}
