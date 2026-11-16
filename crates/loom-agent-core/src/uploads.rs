//! Files that a connection's client uploads to the workspace: each upload
//! writes a temporary file next to its destination, which replaces the
//! destination once all of it arrived.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use loom_proto::{ApiError, ErrorCode, MAX_UPLOAD_CHUNK, MAX_UPLOADS, UploadId};
use loom_tools::{FileAccess, Workspace};

#[derive(Clone)]
struct Upload {
    path: String,
    dest: PathBuf,
    temp: PathBuf,
}

/// The uploads of one connection. Dropping it removes the temporary files of
/// the uploads that did not finish.
#[derive(Default)]
pub struct Uploads {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    open: HashMap<UploadId, Upload>,
    /// Cancelled uploads, whose chunks still on the way are refused.
    cancelled: HashSet<UploadId>,
}

fn bad_request(message: String) -> ApiError {
    ApiError::new(ErrorCode::BadRequest, message)
}

fn io_error(path: &str, err: std::io::Error) -> ApiError {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    };
    ApiError::new(code, format!("{path}: {err}"))
}

fn join_error(err: tokio::task::JoinError) -> ApiError {
    ApiError::new(ErrorCode::Internal, err.to_string())
}

/// Where a new file at the workspace path `path` goes: its directory must
/// exist in the workspace.
fn destination(root: &Path, path: &str) -> Result<PathBuf, ApiError> {
    let relative = Path::new(path);
    let valid = !path.contains('\0')
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    let Some(name) = relative.file_name().filter(|_| valid) else {
        return Err(bad_request(format!("invalid path {path:?}")));
    };
    let parent = root.join(relative.parent().unwrap_or(Path::new("")));
    let dir = parent.canonicalize().map_err(|err| io_error(path, err))?;
    if !dir.starts_with(root) {
        return Err(bad_request(format!("{path} is outside the workspace")));
    }
    if !dir.is_dir() {
        return Err(bad_request(format!("the folder of {path} is not a folder")));
    }
    Ok(dir.join(name))
}

impl Uploads {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes `data` at `offset` of the upload's temporary file, which the
    /// upload's first chunk creates.
    pub async fn chunk(
        &self,
        root: &Path,
        upload: UploadId,
        path: &str,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(), ApiError> {
        if data.len() > MAX_UPLOAD_CHUNK as usize {
            return Err(bad_request(format!(
                "a chunk has at most {MAX_UPLOAD_CHUNK} bytes"
            )));
        }
        if offset.checked_add(data.len() as u64).is_none() {
            return Err(bad_request(format!("invalid offset {offset}")));
        }
        let entry = {
            let mut state = self.lock();
            if state.cancelled.contains(&upload) {
                return Err(ApiError::new(
                    ErrorCode::Cancelled,
                    format!("upload {upload} was cancelled"),
                ));
            }
            match state.open.get(&upload) {
                Some(entry) if entry.path != path => {
                    return Err(bad_request(format!(
                        "upload {upload} is for {}, not {path}",
                        entry.path
                    )));
                }
                Some(entry) => entry.clone(),
                None => {
                    if state.open.len() >= MAX_UPLOADS {
                        return Err(ApiError::new(
                            ErrorCode::Busy,
                            format!("a connection has at most {MAX_UPLOADS} uploads at once"),
                        ));
                    }
                    let dest = destination(root, path)?;
                    let name = dest
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let temp = dest.with_file_name(format!(".{name}.{upload:016x}.upload"));
                    let entry = Upload {
                        path: path.to_owned(),
                        dest,
                        temp,
                    };
                    state.open.insert(upload, entry.clone());
                    entry
                }
            }
        };
        let temp = entry.temp.clone();
        tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&temp)?;
            file.write_all_at(&data, offset)
        })
        .await
        .map_err(join_error)?
        .map_err(|err| io_error(path, err))?;
        // The upload may have been cancelled meanwhile, after its temporary
        // file was removed and before this chunk made it again.
        if !self.lock().open.contains_key(&upload) {
            let _ = std::fs::remove_file(&entry.temp);
            return Err(ApiError::new(
                ErrorCode::Cancelled,
                format!("upload {upload} was cancelled"),
            ));
        }
        Ok(())
    }

    /// Moves the upload's temporary file, which must have `size` bytes, to
    /// its destination. Holds the destination's write lock, so it never
    /// interleaves with a tool call on the same file.
    pub async fn finish(
        &self,
        workspace: &Workspace,
        upload: UploadId,
        size: u64,
        overwrite: bool,
    ) -> Result<(), ApiError> {
        let entry = self
            .lock()
            .open
            .get(&upload)
            .cloned()
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no upload {upload}")))?;
        let _guard = workspace
            .locks()
            .lock_file(&entry.dest, FileAccess::Write)
            .await;
        let moved = entry.clone();
        tokio::task::spawn_blocking(move || {
            let Upload { path, dest, temp } = moved;
            let written = std::fs::metadata(&temp)
                .map_err(|err| io_error(&path, err))?
                .len();
            if written != size {
                return Err(bad_request(format!(
                    "{path} has {written} of its {size} bytes"
                )));
            }
            match std::fs::symlink_metadata(&dest) {
                Ok(_) if !overwrite => {
                    return Err(ApiError::new(
                        ErrorCode::Conflict,
                        format!("{path} already exists"),
                    ));
                }
                Ok(meta) if meta.is_dir() => {
                    return Err(bad_request(format!("{path} is a folder")));
                }
                Ok(meta) if meta.is_file() => {
                    // A replaced file keeps its permissions.
                    std::fs::set_permissions(&temp, meta.permissions())
                        .map_err(|err| io_error(&path, err))?;
                }
                _ => {}
            }
            std::fs::rename(&temp, &dest).map_err(|err| io_error(&path, err))
        })
        .await
        .map_err(join_error)??;
        self.lock().open.remove(&upload);
        Ok(())
    }

    /// Stops an upload and removes its temporary file.
    pub async fn cancel(&self, upload: UploadId) {
        let entry = {
            let mut state = self.lock();
            state.cancelled.insert(upload);
            state.open.remove(&upload)
        };
        let Some(entry) = entry else {
            return;
        };
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(entry.temp)).await;
    }
}

impl Drop for Uploads {
    fn drop(&mut self) {
        for (_, entry) in self.lock().open.drain() {
            let _ = std::fs::remove_file(entry.temp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        (dir, workspace)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn chunks_in_any_order_make_the_file() {
        let (_dir, ws) = workspace();
        let uploads = Uploads::default();
        let root = ws.root();
        uploads
            .chunk(root, 1, "sub/a.bin", 3, b"def".to_vec())
            .await
            .unwrap();
        uploads
            .chunk(root, 1, "sub/a.bin", 0, b"abc".to_vec())
            .await
            .unwrap();
        assert!(!root.join("sub/a.bin").exists());
        let err = uploads.finish(&ws, 1, 7, false).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
        uploads.finish(&ws, 1, 6, false).await.unwrap();
        assert_eq!(std::fs::read(root.join("sub/a.bin")).unwrap(), b"abcdef");
        assert_eq!(names(&root.join("sub")), ["a.bin"]);
    }

    #[tokio::test]
    async fn an_existing_file_is_replaced_only_when_asked() {
        let (_dir, ws) = workspace();
        let root = ws.root();
        std::fs::write(root.join("a.txt"), "old").unwrap();
        let uploads = Uploads::default();
        uploads
            .chunk(root, 2, "a.txt", 0, b"new".to_vec())
            .await
            .unwrap();
        let err = uploads.finish(&ws, 2, 3, false).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"old");
        uploads.finish(&ws, 2, 3, true).await.unwrap();
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"new");
    }

    #[tokio::test]
    async fn bad_paths_are_refused() {
        let (_dir, ws) = workspace();
        let uploads = Uploads::default();
        for path in ["../x", "/etc/x", "", "sub/..", "missing/x"] {
            let result = uploads.chunk(ws.root(), 3, path, 0, Vec::new()).await;
            assert!(result.is_err(), "{path}");
        }
        assert!(uploads.lock().open.is_empty());
    }

    #[tokio::test]
    async fn cancelled_and_dropped_uploads_leave_nothing() {
        let (_dir, ws) = workspace();
        let root = ws.root().to_path_buf();
        let uploads = Uploads::default();
        uploads
            .chunk(&root, 4, "b.bin", 0, b"x".to_vec())
            .await
            .unwrap();
        uploads
            .chunk(&root, 5, "c.bin", 0, b"y".to_vec())
            .await
            .unwrap();
        assert_eq!(names(&root).len(), 3);
        uploads.cancel(4).await;
        assert_eq!(names(&root), [".c.bin.0000000000000005.upload", "sub"]);
        let err = uploads
            .chunk(&root, 4, "b.bin", 1, b"x".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Cancelled);
        let err = uploads
            .chunk(&root, 5, "other.bin", 1, b"z".to_vec())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
        drop(uploads);
        assert_eq!(names(&root), ["sub"]);
    }
}
