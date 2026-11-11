use std::collections::VecDeque;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

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
        mut progress: impl FnMut(u64, u64) + Send,
    ) -> Result<u64> {
        let first = self.read_file_chunk(path, 0, CHUNK).await?;
        let (size, modified) = (first.size, first.modified);
        let temp = temp_file(dest)?;
        let mut out = tokio::fs::File::from_std(temp.as_file().try_clone()?);
        let mut received = check(path, &first, size, modified, 0)?;
        out.write_all(&first.data).await?;
        progress(received, size);

        let mut next = received;
        let mut in_flight = VecDeque::new();
        while received < size {
            while next < size && in_flight.len() < WINDOW {
                let command = Command::ReadFileChunk {
                    path: path.to_owned(),
                    offset: next,
                    len: CHUNK,
                };
                in_flight.push_back((next, self.submit(command).await?));
                next += u64::from(CHUNK).min(size - next);
            }
            let Some((offset, reply)) = in_flight.pop_front() else {
                break;
            };
            let chunk = match reply.await? {
                Payload::FileChunk(chunk) => chunk,
                other => return Err(unexpected(other)),
            };
            received += check(path, &chunk, size, modified, offset)?;
            out.write_all(&chunk.data).await?;
            progress(received, size);
        }
        out.flush().await?;
        drop(out);
        temp.persist(dest)
            .map_err(|err| ClientError::Io(err.error))?;
        Ok(size)
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
