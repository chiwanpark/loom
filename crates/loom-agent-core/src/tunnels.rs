//! TCP connections that the agent opens for a client, which forwards a port
//! of its own through them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use loom_proto::{
    ApiError, ErrorCode, MAX_TUNNEL_DATA, MAX_TUNNELS, Payload, RequestId, ServerMessage,
    TUNNEL_WINDOW, TunnelId,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// How long connecting to a port may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Writes waiting for a tunnel's writer. A client keeps only a few of its
/// data requests unanswered, so a full queue means it does not.
const WRITE_QUEUE: usize = 64;

/// The bytes the agent may still send ahead of the client's acknowledgments.
struct Window {
    available: Mutex<u64>,
    changed: Notify,
}

impl Window {
    fn new() -> Self {
        Self {
            available: Mutex::new(u64::from(TUNNEL_WINDOW)),
            changed: Notify::new(),
        }
    }

    /// Takes up to `max` bytes of room, waiting while there is none.
    async fn take(&self, max: u64) -> u64 {
        loop {
            let notified = self.changed.notified();
            {
                let mut available = self.available.lock().unwrap_or_else(|e| e.into_inner());
                if *available > 0 {
                    let taken = (*available).min(max);
                    *available -= taken;
                    return taken;
                }
            }
            notified.await;
        }
    }

    fn give(&self, bytes: u64) {
        let mut available = self.available.lock().unwrap_or_else(|e| e.into_inner());
        *available = (*available + bytes).min(u64::from(TUNNEL_WINDOW));
        drop(available);
        self.changed.notify_one();
    }
}

enum Write {
    Data { id: RequestId, data: Vec<u8> },
    End { id: RequestId },
}

struct Tunnel {
    writes: mpsc::Sender<Write>,
    window: Arc<Window>,
    reader: Option<JoinHandle<()>>,
    writer: JoinHandle<()>,
    /// Which of the two directions ended.
    read_done: bool,
    write_done: bool,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(reader) = &self.reader {
            reader.abort();
        }
        self.writer.abort();
    }
}

/// The tunnels of one connection.
#[derive(Clone)]
pub struct Tunnels {
    open: Arc<Mutex<HashMap<TunnelId, Tunnel>>>,
    out: mpsc::Sender<ServerMessage>,
}

fn not_found(tunnel: TunnelId) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("no tunnel {tunnel}"))
}

impl Tunnels {
    pub fn new(out: mpsc::Sender<ServerMessage>) -> Self {
        Self {
            open: Arc::default(),
            out,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TunnelId, Tunnel>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Connects tunnel `tunnel` to `host:port` and answers request `id`:
    /// the answer comes before any of the port's data.
    pub async fn open(&self, id: RequestId, tunnel: TunnelId, host: String, port: u16) {
        let result = self.connect(tunnel, &host, port).await;
        let (result, read) = match result {
            Ok(read) => (Ok(Payload::Ok), Some(read)),
            Err(err) => (Err(err), None),
        };
        let _ = self.out.send(ServerMessage::Response { id, result }).await;
        let Some(read) = read else {
            return;
        };
        let mut open = self.lock();
        if let Some(entry) = open.get_mut(&tunnel) {
            let window = entry.window.clone();
            let this = self.clone();
            entry.reader = Some(tokio::spawn(async move {
                let error = this.read(tunnel, read, window).await.err();
                let _ = this
                    .out
                    .send(ServerMessage::TunnelEnd {
                        tunnel,
                        error: error.clone(),
                    })
                    .await;
                if error.is_some() {
                    this.lock().remove(&tunnel);
                } else {
                    this.finish(tunnel, true);
                }
            }));
        }
    }

    async fn connect(
        &self,
        tunnel: TunnelId,
        host: &str,
        port: u16,
    ) -> Result<OwnedReadHalf, ApiError> {
        {
            let open = self.lock();
            if open.contains_key(&tunnel) {
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    format!("tunnel {tunnel} is open already"),
                ));
            }
            if open.len() >= MAX_TUNNELS {
                return Err(ApiError::new(
                    ErrorCode::Busy,
                    format!("a connection has at most {MAX_TUNNELS} tunnels"),
                ));
            }
        }
        let stream =
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    return Err(ApiError::new(
                        ErrorCode::NotFound,
                        format!("could not connect to {host}:{port}: {err}"),
                    ));
                }
                Err(_) => {
                    return Err(ApiError::new(
                        ErrorCode::NotFound,
                        format!("connecting to {host}:{port} timed out"),
                    ));
                }
            };
        let _ = stream.set_nodelay(true);
        let (read, write) = stream.into_split();
        let (writes, queue) = mpsc::channel(WRITE_QUEUE);
        let this = self.clone();
        let writer = tokio::spawn(async move { this.write(tunnel, write, queue).await });
        let mut open = self.lock();
        if open.contains_key(&tunnel) {
            writer.abort();
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!("tunnel {tunnel} is open already"),
            ));
        }
        open.insert(
            tunnel,
            Tunnel {
                writes,
                window: Arc::new(Window::new()),
                reader: None,
                writer,
                read_done: false,
                write_done: false,
            },
        );
        Ok(read)
    }

    /// Sends what the port sends, as far as the window allows, until it ends.
    async fn read(
        &self,
        tunnel: TunnelId,
        mut read: OwnedReadHalf,
        window: Arc<Window>,
    ) -> Result<(), String> {
        let mut buffer = vec![0u8; MAX_TUNNEL_DATA as usize];
        loop {
            let room = window.take(u64::from(MAX_TUNNEL_DATA)).await as usize;
            let read = read
                .read(&mut buffer[..room])
                .await
                .map_err(|err| err.to_string())?;
            if read < room {
                window.give((room - read) as u64);
            }
            if read == 0 {
                return Ok(());
            }
            let message = ServerMessage::TunnelData {
                tunnel,
                data: buffer[..read].to_vec(),
            };
            if self.out.send(message).await.is_err() {
                return Err("the connection closed".into());
            }
        }
    }

    /// Writes the client's bytes to the port in order, answering each request.
    async fn write(
        &self,
        tunnel: TunnelId,
        mut write: OwnedWriteHalf,
        mut queue: mpsc::Receiver<Write>,
    ) {
        while let Some(next) = queue.recv().await {
            match next {
                Write::Data { id, data } => {
                    let result = write.write_all(&data).await;
                    let failed = result.is_err();
                    let result = result.map(|()| Payload::Ok).map_err(|err| {
                        ApiError::new(
                            ErrorCode::Internal,
                            format!("could not write to the port: {err}"),
                        )
                    });
                    let error = result.as_ref().err().map(|err| err.message.clone());
                    let _ = self.out.send(ServerMessage::Response { id, result }).await;
                    if failed {
                        let _ = self
                            .out
                            .send(ServerMessage::TunnelEnd { tunnel, error })
                            .await;
                        self.lock().remove(&tunnel);
                        return;
                    }
                }
                Write::End { id } => {
                    let _ = write.shutdown().await;
                    let _ = self
                        .out
                        .send(ServerMessage::Response {
                            id,
                            result: Ok(Payload::Ok),
                        })
                        .await;
                    self.finish(tunnel, false);
                    return;
                }
            }
        }
    }

    /// Marks one direction of a tunnel ended, and forgets the tunnel once
    /// both are.
    fn finish(&self, tunnel: TunnelId, read: bool) {
        let mut open = self.lock();
        let Some(entry) = open.get_mut(&tunnel) else {
            return;
        };
        if read {
            entry.read_done = true;
            entry.reader = None;
        } else {
            entry.write_done = true;
        }
        // The task that called this returns without waiting again, so the
        // abort of dropping the tunnel does not cut it short.
        if entry.read_done && entry.write_done {
            open.remove(&tunnel);
        }
    }

    /// Queues bytes for the port; the writer answers request `id`.
    pub fn data(&self, id: RequestId, tunnel: TunnelId, data: Vec<u8>) -> Result<(), ApiError> {
        if data.len() > MAX_TUNNEL_DATA as usize {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("tunnel data holds at most {MAX_TUNNEL_DATA} bytes"),
            ));
        }
        self.queue(tunnel, Write::Data { id, data })
    }

    /// Queues the end of the client's bytes; the writer answers request `id`.
    pub fn end(&self, id: RequestId, tunnel: TunnelId) -> Result<(), ApiError> {
        self.queue(tunnel, Write::End { id })
    }

    fn queue(&self, tunnel: TunnelId, write: Write) -> Result<(), ApiError> {
        let open = self.lock();
        let entry = open.get(&tunnel).ok_or_else(|| not_found(tunnel))?;
        if entry.write_done {
            return Err(ApiError::new(
                ErrorCode::BadRequest,
                format!("tunnel {tunnel} was ended for writing"),
            ));
        }
        entry.writes.try_send(write).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => ApiError::new(
                ErrorCode::Busy,
                format!("tunnel {tunnel} has too many writes waiting"),
            ),
            mpsc::error::TrySendError::Closed(_) => not_found(tunnel),
        })
    }

    pub fn ack(&self, tunnel: TunnelId, bytes: u32) -> Result<(), ApiError> {
        let open = self.lock();
        let entry = open.get(&tunnel).ok_or_else(|| not_found(tunnel))?;
        entry.window.give(u64::from(bytes));
        Ok(())
    }

    pub fn close(&self, tunnel: TunnelId) -> Result<(), ApiError> {
        self.lock()
            .remove(&tunnel)
            .map(drop)
            .ok_or_else(|| not_found(tunnel))
    }

    /// Closes every tunnel, as when the connection goes away.
    pub fn close_all(&self) {
        self.lock().clear();
    }

    /// How many tunnels are open.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
