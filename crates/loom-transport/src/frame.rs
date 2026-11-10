use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

pub use loom_proto::MAX_FRAME_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerInfo {
    Memory,
    Local { uid: u32, pid: Option<i32> },
    Command { program: String },
}

type Guard = Arc<dyn std::any::Any + Send + Sync>;
type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_type::<u32>()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<String>>);

impl Diagnostics {
    const LIMIT: usize = 8192;

    pub fn push(&self, text: &str) {
        let mut buf = self.0.lock().unwrap_or_else(|e| e.into_inner());
        buf.push_str(text);
        if buf.len() > Self::LIMIT {
            let mut cut = buf.len() - Self::LIMIT;
            while !buf.is_char_boundary(cut) {
                cut += 1;
            }
            buf.drain(..cut);
        }
    }

    pub fn text(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct FrameSender {
    inner: FramedWrite<BoxWrite, LengthDelimitedCodec>,
    _guard: Option<Guard>,
}

impl FrameSender {
    pub async fn send(&mut self, frame: Bytes) -> io::Result<()> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame of {} bytes exceeds the limit", frame.len()),
            ));
        }
        self.inner.send(frame).await
    }

    pub async fn close(&mut self) -> io::Result<()> {
        SinkExt::<Bytes>::close(&mut self.inner).await
    }
}

pub struct FrameReceiver {
    inner: FramedRead<BoxRead, LengthDelimitedCodec>,
    _guard: Option<Guard>,
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> io::Result<Option<Bytes>> {
        match self.inner.next().await {
            Some(Ok(frame)) => Ok(Some(frame.freeze())),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}

pub struct FrameStream {
    sender: FrameSender,
    receiver: FrameReceiver,
    peer: PeerInfo,
    diagnostics: Option<Diagnostics>,
}

impl FrameStream {
    pub fn new<R, W>(reader: R, writer: W, peer: PeerInfo) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::with_guard(reader, writer, peer, None, None)
    }

    pub(crate) fn with_guard<R, W>(
        reader: R,
        writer: W,
        peer: PeerInfo,
        guard: Option<Guard>,
        diagnostics: Option<Diagnostics>,
    ) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            sender: FrameSender {
                inner: FramedWrite::new(Box::new(writer), codec()),
                _guard: guard.clone(),
            },
            receiver: FrameReceiver {
                inner: FramedRead::new(Box::new(reader), codec()),
                _guard: guard,
            },
            peer,
            diagnostics,
        }
    }

    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    pub fn diagnostics(&self) -> Option<Diagnostics> {
        self.diagnostics.clone()
    }

    pub async fn send(&mut self, frame: Bytes) -> io::Result<()> {
        self.sender.send(frame).await
    }

    pub async fn recv(&mut self) -> io::Result<Option<Bytes>> {
        self.receiver.recv().await
    }

    pub fn split(self) -> (FrameSender, FrameReceiver, PeerInfo) {
        (self.sender, self.receiver, self.peer)
    }
}

pub fn memory_pair() -> (FrameStream, FrameStream) {
    let (a, b) = tokio::io::duplex(256 * 1024);
    let (a_read, a_write) = tokio::io::split(a);
    let (b_read, b_write) = tokio::io::split(b);
    (
        FrameStream::new(a_read, a_write, PeerInfo::Memory),
        FrameStream::new(b_read, b_write, PeerInfo::Memory),
    )
}
