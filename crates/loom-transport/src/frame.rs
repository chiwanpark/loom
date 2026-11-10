use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use loom_crypto::CryptoError;
use loom_crypto::SecureChannel;
use loom_crypto::noise::{SecureReceiver, SecureSender};
pub use loom_proto::MAX_FRAME_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerInfo {
    Memory,
    Local { uid: u32, pid: Option<i32> },
    Command { program: String },
    Relay { label: String, pubkey: [u8; 32] },
}

pub type Guard = Arc<dyn std::any::Any + Send + Sync>;
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

enum SenderInner {
    Framed(FramedWrite<BoxWrite, LengthDelimitedCodec>),
    Secure(Option<SecureSender>),
}

enum ReceiverInner {
    Framed(FramedRead<BoxRead, LengthDelimitedCodec>),
    Secure(SecureReceiver),
}

pub struct FrameSender {
    inner: SenderInner,
    _guard: Option<Guard>,
}

fn crypto_error(err: CryptoError) -> io::Error {
    match err {
        CryptoError::Closed => io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()),
        CryptoError::Io(err) => err,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

impl FrameSender {
    pub async fn send(&mut self, frame: Bytes) -> io::Result<()> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame of {} bytes exceeds the limit", frame.len()),
            ));
        }
        match &mut self.inner {
            SenderInner::Framed(inner) => inner.send(frame).await,
            SenderInner::Secure(Some(sender)) => sender.send(&frame).await.map_err(crypto_error),
            SenderInner::Secure(None) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the stream is closed",
            )),
        }
    }

    pub async fn close(&mut self) -> io::Result<()> {
        match &mut self.inner {
            SenderInner::Framed(inner) => SinkExt::<Bytes>::close(inner).await,
            SenderInner::Secure(sender) => {
                sender.take();
                Ok(())
            }
        }
    }
}

pub struct FrameReceiver {
    inner: ReceiverInner,
    _guard: Option<Guard>,
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> io::Result<Option<Bytes>> {
        match &mut self.inner {
            ReceiverInner::Framed(inner) => match inner.next().await {
                Some(Ok(frame)) => Ok(Some(frame.freeze())),
                Some(Err(err)) => Err(err),
                None => Ok(None),
            },
            ReceiverInner::Secure(receiver) => match receiver.recv().await {
                Ok(frame) => Ok(frame.map(Bytes::from)),
                Err(err) => Err(crypto_error(err)),
            },
        }
    }
}

impl std::fmt::Debug for FrameStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStream")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
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
                inner: SenderInner::Framed(FramedWrite::new(Box::new(writer), codec())),
                _guard: guard.clone(),
            },
            receiver: FrameReceiver {
                inner: ReceiverInner::Framed(FramedRead::new(Box::new(reader), codec())),
                _guard: guard,
            },
            peer,
            diagnostics,
        }
    }

    pub fn secure(channel: SecureChannel, peer: PeerInfo, guard: Option<Guard>) -> Self {
        let (sender, receiver) = channel.split();
        Self {
            sender: FrameSender {
                inner: SenderInner::Secure(Some(sender)),
                _guard: guard.clone(),
            },
            receiver: FrameReceiver {
                inner: ReceiverInner::Secure(receiver),
                _guard: guard,
            },
            peer,
            diagnostics: None,
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
