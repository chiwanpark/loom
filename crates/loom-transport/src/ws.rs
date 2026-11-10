use std::time::Duration;

use futures::{SinkExt, StreamExt};
use loom_proto::relay::RelayFrame;
use loom_proto::{decode, encode};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::relay::RelayError;

pub const HEARTBEAT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const QUEUE: usize = 256;
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl AccessToken {
    pub fn from_env(id_var: &str, secret_var: &str) -> Result<Self, String> {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{name} is not set"))
        };
        Ok(Self {
            client_id: read(id_var)?,
            client_secret: read(secret_var)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    pub url: String,
    pub access: Option<AccessToken>,
}

impl RelayEndpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            access: None,
        }
    }

    fn url_for(&self, path: &str) -> String {
        format!("{}{path}", self.url.trim_end_matches('/'))
    }
}

pub(crate) struct WsLink {
    pub tx: mpsc::Sender<RelayFrame>,
    pub rx: mpsc::Receiver<RelayFrame>,
    pub _task: AbortOnDrop,
}

pub struct AbortOnDrop(pub JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) async fn connect(endpoint: &RelayEndpoint, path: &str) -> Result<WsLink, RelayError> {
    let url = endpoint.url_for(path);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| RelayError::Connect(format!("{url}: {e}")))?;
    if let Some(access) = &endpoint.access {
        let header = |value: &str| {
            HeaderValue::from_str(value)
                .map_err(|_| RelayError::Connect("the access token has invalid characters".into()))
        };
        request
            .headers_mut()
            .insert("CF-Access-Client-Id", header(&access.client_id)?);
        request
            .headers_mut()
            .insert("CF-Access-Client-Secret", header(&access.client_secret)?);
    }
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (socket, _) = tokio::time::timeout(
        Duration::from_secs(30),
        tokio_tungstenite::connect_async_with_config(request, Some(config), true),
    )
    .await
    .map_err(|_| RelayError::Connect(format!("{url}: timed out")))?
    .map_err(|e| match e {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            RelayError::Connect(format!("{url}: the relay answered {}", response.status()))
        }
        other => RelayError::Connect(format!("{url}: {other}")),
    })?;
    let (out_tx, mut out_rx) = mpsc::channel::<RelayFrame>(QUEUE);
    let (in_tx, in_rx) = mpsc::channel::<RelayFrame>(QUEUE);
    let task = tokio::spawn(async move {
        let (mut sink, mut stream) = socket.split();
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        let mut last_seen = tokio::time::Instant::now();
        loop {
            tokio::select! {
                frame = out_rx.recv() => {
                    let Some(frame) = frame else {
                        let _ = sink.send(Message::Close(None)).await;
                        break;
                    };
                    let Ok(bytes) = encode(&frame) else { break };
                    if let Err(err) = sink.send(Message::Binary(bytes.into())).await {
                        tracing::debug!(error = %err, "relay write failed");
                        break;
                    }
                }
                message = stream.next() => {
                    last_seen = tokio::time::Instant::now();
                    match message {
                        Some(Ok(Message::Binary(bytes))) => {
                            let Ok(frame) = decode::<RelayFrame>(&bytes) else {
                                tracing::warn!("the relay sent a malformed frame");
                                break;
                            };
                            if in_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(err)) => {
                            tracing::debug!(error = %err, "relay read failed");
                            break;
                        }
                        Some(Ok(_)) => {}
                    }
                }
                _ = heartbeat.tick() => {
                    if last_seen.elapsed() > IDLE_TIMEOUT {
                        tracing::warn!("the relay connection went idle");
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(WsLink {
        tx: out_tx,
        rx: in_rx,
        _task: AbortOnDrop(task),
    })
}
