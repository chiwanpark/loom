use std::collections::VecDeque;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::pin::Pin;

use loom_proto::{Command, FileChunk, Payload};
use tokio::io::AsyncWriteExt;

use crate::client::{Client, unexpected};
use crate::error::{ClientError, Result};

/// The bytes asked for per request. Small enough that a chunk does not hold
/// up other messages on the connection for long.
const CHUNK: u32 = 1024 * 1024;
/// Requests in flight at once, so the transfer does not wait a round trip
/// for every chunk.
const WINDOW: usize = 4;

impl Client {
    /// Bytes of a workspace file from `offset` on; see
    /// [`Command::ReadFileChunk`].
    pub async fn read_file_chunk(&self, path: &str, offset: u64, len: u32) -> Result<FileChunk> {
        let command = Command::ReadFileChunk {
            path: path.to_owned(),
            offset,
            len,
            session: self.scope.clone(),
        };
        match self.request(command).await? {
            Payload::FileChunk(chunk) => Ok(chunk),
            other => Err(unexpected(other)),
        }
    }

    /// Copies a workspace file to `dest` on this machine and returns its size.
    /// `progress` hears the bytes received so far and the total after every
    /// chunk. The bytes go to a temporary file next to `dest`, which replaces
    /// `dest` only once all of them arrived; a failed or dropped download
    /// removes it. Fails with [`ClientError::Changed`] when the file changes
    /// while it is copied.
    pub async fn download_file(
        &self,
        path: &str,
        dest: &Path,
        progress: impl FnMut(u64, u64) + Send,
    ) -> Result<u64> {
        self.download_file_within(path, dest, u64::MAX, progress)
            .await
    }

    /// Like [`Client::download_file`], but fails with
    /// [`ClientError::TooLarge`], before anything is written, when the file
    /// has more than `max` bytes.
    pub async fn download_file_within(
        &self,
        path: &str,
        dest: &Path,
        max: u64,
        mut progress: impl FnMut(u64, u64) + Send,
    ) -> Result<u64> {
        let mut chunks = Chunks::start(self, path, max).await?;
        let temp = temp_file(dest)?;
        let mut out = tokio::fs::File::from_std(temp.as_file().try_clone()?);
        while let Some(data) = chunks.next().await? {
            out.write_all(&data).await?;
            progress(chunks.received, chunks.size);
        }
        out.flush().await?;
        drop(out);
        temp.persist(dest)
            .map_err(|err| ClientError::Io(err.error))?;
        Ok(chunks.size)
    }

    /// The bytes of a workspace file, as for a viewer. Fails with
    /// [`ClientError::TooLarge`] when the file has more than `max` bytes, and
    /// with [`ClientError::Changed`] when it changes while it is read.
    pub async fn read_file_bytes(&self, path: &str, max: u64) -> Result<Vec<u8>> {
        let mut chunks = Chunks::start(self, path, max).await?;
        let mut bytes = Vec::with_capacity(usize::try_from(chunks.size).unwrap_or(0));
        while let Some(data) = chunks.next().await? {
            bytes.extend_from_slice(&data);
        }
        Ok(bytes)
    }
}

/// The answer to a request in flight.
type Reply = Pin<Box<dyn Future<Output = Result<Payload>> + Send>>;

/// A workspace file read chunk by chunk, in order, with a few requests in
/// flight so the transfer does not wait a round trip for every chunk.
struct Chunks<'a> {
    client: &'a Client,
    path: &'a str,
    size: u64,
    modified: Option<u64>,
    first: Option<Vec<u8>>,
    received: u64,
    next: u64,
    in_flight: VecDeque<(u64, Reply)>,
}

impl<'a> Chunks<'a> {
    /// Reads the first chunk, which tells the file's size; a file of more
    /// than `max` bytes is not read.
    async fn start(client: &'a Client, path: &'a str, max: u64) -> Result<Self> {
        let first = client.read_file_chunk(path, 0, CHUNK).await?;
        let (size, modified) = (first.size, first.modified);
        if size > max {
            return Err(ClientError::TooLarge {
                path: path.to_owned(),
                size,
                max,
            });
        }
        let received = check(path, &first, size, modified, 0)?;
        Ok(Self {
            client,
            path,
            size,
            modified,
            first: Some(first.data),
            received,
            next: received,
            in_flight: VecDeque::new(),
        })
    }

    /// The next chunk's bytes, or `None` after the last one.
    async fn next(&mut self) -> Result<Option<Vec<u8>>> {
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        if self.received >= self.size {
            return Ok(None);
        }
        while self.next < self.size && self.in_flight.len() < WINDOW {
            let command = Command::ReadFileChunk {
                path: self.path.to_owned(),
                offset: self.next,
                len: CHUNK,
                session: self.client.scope.clone(),
            };
            self.in_flight
                .push_back((self.next, Box::pin(self.client.submit(command).await?)));
            self.next += u64::from(CHUNK).min(self.size - self.next);
        }
        let Some((offset, reply)) = self.in_flight.pop_front() else {
            return Ok(None);
        };
        let chunk = match reply.await? {
            Payload::FileChunk(chunk) => chunk,
            other => return Err(unexpected(other)),
        };
        self.received += check(self.path, &chunk, self.size, self.modified, offset)?;
        Ok(Some(chunk.data))
    }
}

/// A file next to `dest` with the permissions a new file gets there.
fn temp_file(dest: &Path) -> Result<tempfile::NamedTempFile> {
    let dir = match dest.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".part")
        .permissions(Permissions::from_mode(0o666))
        .tempfile_in(dir)?)
}

/// The length of a chunk that belongs at `offset` of a file that still has
/// the `size` and `modified` time seen first.
fn check(
    path: &str,
    chunk: &FileChunk,
    size: u64,
    modified: Option<u64>,
    offset: u64,
) -> Result<u64> {
    let expected = u64::from(CHUNK).min(size - offset);
    let len = chunk.data.len() as u64;
    if chunk.size != size || chunk.modified != modified || len != expected {
        return Err(ClientError::Changed(path.to_owned()));
    }
    Ok(len)
}
