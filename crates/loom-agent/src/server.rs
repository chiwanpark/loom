use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use loom_agent_core::{Agent, Connection, Flow, Supervisor};
use loom_proto::{AgentInfo, ApiError, ClientMessage, ErrorCode, ServerMessage, decode, encode};
use loom_transport::{FrameStream, UnixFrameListener};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const OUTGOING_QUEUE: usize = 1024;
const FLUSH_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct ServerContext {
    pub agent: Agent,
    pub info: AgentInfo,
    pub supervisor: Option<Arc<dyn Supervisor>>,
}

impl ServerContext {
    pub fn new(agent: Agent, info: AgentInfo) -> Self {
        Self {
            agent,
            info,
            supervisor: None,
        }
    }
}

pub async fn serve_stream(ctx: ServerContext, stream: FrameStream) {
    let peer = stream.peer().clone();
    let (mut sender, mut receiver, _) = stream.split();
    let (out, mut outgoing) = mpsc::channel::<ServerMessage>(OUTGOING_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let frame = match encode(&message) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::error!(error = %err, "could not encode a server message");
                    continue;
                }
            };
            if let Err(err) = sender.send(Bytes::from(frame)).await {
                tracing::debug!(error = %err, "client write failed");
                return;
            }
        }
        let _ = sender.close().await;
    });

    let mut connection = Connection::new(ctx.agent, ctx.info, out).with_supervisor(ctx.supervisor);
    tracing::debug!(?peer, "connection opened");
    loop {
        let frame = match receiver.recv().await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(error = %err, "client read failed");
                break;
            }
        };
        let message: ClientMessage = match decode(&frame) {
            Ok(message) => message,
            Err(err) => {
                connection
                    .reject(ApiError::new(ErrorCode::Protocol, err.to_string()))
                    .await;
                break;
            }
        };
        if connection.handle(message).await == Flow::Close {
            break;
        }
    }
    drop(connection);
    let _ = writer.await;
    tracing::debug!(?peer, "connection closed");
}

pub async fn run_listener(
    ctx: ServerContext,
    listener: UnixFrameListener,
    shutdown: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    connections.spawn(serve_stream(ctx.clone(), stream));
                }
                Err(err) => {
                    tracing::error!(error = %err, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    ctx.agent.shutdown().await;
    let _ = tokio::time::timeout(FLUSH_GRACE, async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.shutdown().await;
}
