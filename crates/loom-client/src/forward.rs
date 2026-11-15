//! Forwarding a local TCP port to a port on the agent's host, through
//! tunnels on the connection to the agent.

use std::collections::VecDeque;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use loom_proto::{Command, ListeningPort, MAX_TUNNEL_DATA, Payload, TUNNEL_WINDOW, TunnelId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::client::{Client, TunnelEvent, unexpected};
use crate::error::{ClientError, Result};

/// The client acknowledges a tunnel's data after it wrote this many bytes.
const ACK_EVERY: u32 = TUNNEL_WINDOW / 4;

/// Data requests a tunnel keeps unanswered at once.
const UNANSWERED: usize = 8;

type Reply = Pin<Box<dyn Future<Output = Result<Payload>> + Send>>;

impl Client {
    /// The TCP ports that programs listen on, on the agent's host; see
    /// [`Command::ListPorts`].
    pub async fn list_ports(&self) -> Result<Vec<ListeningPort>> {
        match self.request(Command::ListPorts).await? {
            Payload::Ports(ports) => Ok(ports),
            other => Err(unexpected(other)),
        }
    }

    /// Connects a tunnel to `host:port` as the agent's host sees it.
    pub async fn open_tunnel(&self, host: &str, port: u16) -> Result<Tunnel> {
        let id = self.next_tunnel.fetch_add(1, Ordering::Relaxed);
        let (sender, events) = mpsc::unbounded_channel();
        match self
            .tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            Some(routes) => routes.insert(id, sender),
            None => return Err(ClientError::Closed),
        };
        let mut tunnel = Tunnel {
            client: self.clone(),
            id,
            events,
            open: false,
        };
        self.request(Command::OpenTunnel {
            tunnel: id,
            host: host.to_owned(),
            port,
        })
        .await?;
        tunnel.open = true;
        Ok(tunnel)
    }
}

/// A TCP connection that the agent opened for this client.
pub struct Tunnel {
    client: Client,
    id: TunnelId,
    events: mpsc::UnboundedReceiver<TunnelEvent>,
    /// Whether the agent may still hold the tunnel, which is closed when
    /// this is dropped.
    open: bool,
}

impl Tunnel {
    /// Carries a local connection's bytes through the tunnel, both ways,
    /// until both sides ended or one failed.
    pub async fn pipe(mut self, stream: TcpStream) -> Result<()> {
        let _ = stream.set_nodelay(true);
        let (mut read, mut write) = stream.into_split();
        let (client, id) = (self.client.clone(), self.id);
        let events = &mut self.events;
        let up = async {
            let mut buffer = vec![0u8; MAX_TUNNEL_DATA as usize];
            let mut unanswered: VecDeque<Reply> = VecDeque::new();
            loop {
                while unanswered.len() >= UNANSWERED {
                    if let Some(reply) = unanswered.pop_front() {
                        reply.await?;
                    }
                }
                let count = read.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                let command = Command::TunnelData {
                    tunnel: id,
                    data: buffer[..count].to_vec(),
                };
                unanswered.push_back(Box::pin(client.submit(command).await?));
            }
            for reply in unanswered {
                reply.await?;
            }
            client.request(Command::TunnelEnd { tunnel: id }).await?;
            Ok::<_, ClientError>(())
        };
        let down = async {
            let mut unacknowledged = 0u32;
            while let Some(event) = events.recv().await {
                match event {
                    TunnelEvent::Data(data) => {
                        write.write_all(&data).await?;
                        unacknowledged += data.len() as u32;
                        if unacknowledged >= ACK_EVERY {
                            // The answer does not matter; a lost tunnel ends
                            // on its own.
                            drop(
                                client
                                    .submit(Command::TunnelAck {
                                        tunnel: id,
                                        bytes: unacknowledged,
                                    })
                                    .await?,
                            );
                            unacknowledged = 0;
                        }
                    }
                    TunnelEvent::End(None) => {
                        let _ = write.shutdown().await;
                        return Ok(());
                    }
                    TunnelEvent::End(Some(error)) => return Err(ClientError::Tunnel(error)),
                }
            }
            Err(ClientError::Closed)
        };
        let result = tokio::try_join!(up, down).map(|_| ());
        // Both sides ended, so the agent forgot the tunnel.
        if result.is_ok() {
            self.open = false;
        }
        result
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(routes) = self
            .client
            .tunnels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            routes.remove(&self.id);
        }
        if self.open {
            self.client
                .send_detached(Command::CloseTunnel { tunnel: self.id });
        }
    }
}

/// A local port that forwards every connection to it to a port on the
/// agent's host, until it is dropped.
pub struct PortForward {
    local: SocketAddr,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl PortForward {
    /// Listens on `127.0.0.1:local` and forwards each connection to
    /// `host:port` on the agent's host, through the client that `client`
    /// gives when it comes, so it goes on after a reconnect. Without
    /// `local`, it listens on `port` when that is free and on any free port
    /// otherwise.
    pub async fn start(
        local: Option<u16>,
        host: String,
        port: u16,
        client: impl Fn() -> Option<Client> + Send + Sync + 'static,
    ) -> Result<Self> {
        let listener = match local {
            Some(local) => TcpListener::bind((Ipv4Addr::LOCALHOST, local)).await?,
            None => match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(listener) => listener,
                Err(_) => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?,
            },
        };
        let local = listener.local_addr()?;
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let task = tokio::spawn(async move {
            let mut pipes = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        let Some(client) = client() else {
                            // Not connected: the connection closes at once.
                            continue;
                        };
                        let (host, count) = (host.clone(), count.clone());
                        count.fetch_add(1, Ordering::Relaxed);
                        pipes.spawn(async move {
                            let result = match client.open_tunnel(&host, port).await {
                                Ok(tunnel) => tunnel.pipe(stream).await,
                                Err(err) => Err(err),
                            };
                            if let Err(err) = result {
                                tracing::debug!(error = %err, port, "a forwarded connection failed");
                            }
                            count.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Some(_) = pipes.join_next(), if !pipes.is_empty() => {}
                }
            }
        });
        Ok(Self {
            local,
            connections,
            task,
        })
    }

    /// The local address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// How many connections it carries now.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        // Aborting the task drops its JoinSet, which aborts every pipe.
        self.task.abort();
    }
}
