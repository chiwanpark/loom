use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;

use loom_proto::{Command, Payload, UploadId};
use tokio::io::AsyncReadExt;

use crate::client::{Client, unexpected};
use crate::error::Result;

/// The bytes sent per request. Small enough that a chunk does not hold up
/// other messages on the connection for long.
const CHUNK: usize = 1024 * 1024;
/// Requests in flight at once, so the transfer does not wait a round trip
/// for every chunk.
const WINDOW: usize = 4;

/// The answer to a request in flight.
type Reply = Pin<Box<dyn Future<Output = Result<Payload>> + Send>>;

/// Cancels an upload on the agent unless it finished, also when the upload
/// is dropped halfway.
struct Pending<'a> {
    client: &'a Client,
    upload: UploadId,
    finished: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.client.send_detached(Command::CancelUpload {
                upload: self.upload,
            });
        }
    }
}

impl Client {
    /// Copies the file `src` of this machine to the workspace path `path`,
    /// whose folder must exist, and returns its size. `progress` hears the
    /// bytes sent so far and the total after every chunk. The file appears
    /// on the agent only once all of it arrived; a failed or dropped upload
    /// leaves nothing behind. Fails with a `Conflict` error when `path`
    /// exists and `overwrite` is not set.
    pub async fn upload_file(
        &self,
        src: &Path,
        path: &str,
        overwrite: bool,
        mut progress: impl FnMut(u64, u64) + Send,
    ) -> Result<u64> {
        let mut file = tokio::fs::File::open(src).await?;
        let size = file.metadata().await?.len();
        let mut pending = Pending {
            client: self,
            upload: rand::random(),
            finished: false,
        };
        let mut in_flight: VecDeque<(u64, Reply)> = VecDeque::new();
        let mut offset = 0u64;
        let mut sent = 0u64;
        // An empty file still needs a chunk, which creates it.
        let mut first = true;
        loop {
            while in_flight.len() < WINDOW && (first || offset < size) {
                let mut data = Vec::with_capacity(CHUNK.min(size.saturating_sub(offset) as usize));
                (&mut file)
                    .take(CHUNK as u64)
                    .read_to_end(&mut data)
                    .await?;
                let len = data.len() as u64;
                if len == 0 && !first {
                    // The file got shorter while it was read.
                    break;
                }
                let command = Command::UploadChunk {
                    upload: pending.upload,
                    path: path.to_owned(),
                    offset,
                    data,
                    session: self.scope.clone(),
                };
                in_flight.push_back((len, Box::pin(self.submit(command).await?)));
                offset += len;
                first = false;
            }
            let Some((len, reply)) = in_flight.pop_front() else {
                break;
            };
            match reply.await? {
                Payload::Ok => {}
                other => return Err(unexpected(other)),
            }
            sent += len;
            progress(sent, size.max(offset));
        }
        let command = Command::FinishUpload {
            upload: pending.upload,
            size: offset,
            overwrite,
            session: self.scope.clone(),
        };
        match self.request(command).await? {
            Payload::Ok => {}
            other => return Err(unexpected(other)),
        }
        pending.finished = true;
        Ok(offset)
    }
}
