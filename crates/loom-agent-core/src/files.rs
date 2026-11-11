use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use loom_proto::{
    ApiError, DirEntry, DirectoryListing, EntryKind, ErrorCode, FileChunk, FileContent,
    FileVersion, MAX_FILE_CHUNK,
};
use loom_tools::{FileAccess, Workspace};
use sha2::{Digest, Sha256};

const MAX_ENTRIES: usize = 5000;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const BINARY_SNIFF: usize = 8000;

fn bad_path(path: &str) -> ApiError {
    ApiError::new(ErrorCode::BadRequest, format!("invalid path {path:?}"))
}

fn io_error(path: &str, err: std::io::Error) -> ApiError {
    let code = match err.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    };
    ApiError::new(code, format!("{path}: {err}"))
}

fn resolve(root: &Path, path: &str) -> Result<PathBuf, ApiError> {
    if path.contains('\0') {
        return Err(bad_path(path));
    }
    let relative = Path::new(path);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(bad_path(path));
    }
    let joined = root.join(relative);
    let resolved = joined.canonicalize().map_err(|err| io_error(path, err))?;
    if !resolved.starts_with(root) {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("{path} is outside the workspace"),
        ));
    }
    Ok(resolved)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        name.to_string()
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

fn read_entries(dir: &Path) -> std::io::Result<(Vec<DirEntry>, bool)> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        let (kind, size) = match std::fs::metadata(entry.path()) {
            Ok(meta) if meta.is_dir() => (EntryKind::Directory, 0),
            Ok(meta) if meta.is_file() => (EntryKind::File, meta.len()),
            _ => (EntryKind::Other, 0),
        };
        entries.push(DirEntry {
            name,
            kind,
            symlink,
            size,
            ignored: false,
        });
    }
    entries.sort_by(|a, b| {
        let dir = |e: &DirEntry| e.kind != EntryKind::Directory;
        (dir(a), a.name.to_lowercase(), &a.name).cmp(&(dir(b), b.name.to_lowercase(), &b.name))
    });
    Ok((entries, truncated))
}

pub async fn list_directory(root: &Path, path: &str) -> Result<DirectoryListing, ApiError> {
    let dir = if path.is_empty() || path == "." {
        root.to_path_buf()
    } else {
        resolve(root, path)?
    };
    let (mut entries, truncated) = tokio::task::spawn_blocking(move || read_entries(&dir))
        .await
        .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?
        .map_err(|err| io_error(path, err))?;
    let paths: Vec<String> = entries.iter().map(|e| join(path, &e.name)).collect();
    let ignored = crate::git::ignored(root, &paths).await;
    for (entry, full) in entries.iter_mut().zip(&paths) {
        entry.ignored = ignored.contains(full);
    }
    Ok(DirectoryListing {
        path: path.to_string(),
        entries,
        truncated,
    })
}

fn version(bytes: &[u8]) -> FileVersion {
    FileVersion(Sha256::digest(bytes).into())
}

fn read_text(file: &Path, path: &str) -> std::io::Result<FileContent> {
    let size = std::fs::metadata(file)?.len();
    let mut data = Vec::new();
    std::fs::File::open(file)?
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut data)?;
    let mut content = FileContent {
        path: path.to_string(),
        size,
        ..FileContent::default()
    };
    if data[..data.len().min(BINARY_SNIFF)].contains(&0) {
        content.binary = true;
        return Ok(content);
    }
    content.truncated = size > data.len() as u64;
    if content.truncated
        && let Some(end) = data.iter().rposition(|b| *b == b'\n')
    {
        data.truncate(end + 1);
    }
    content.version = version(&data);
    content.valid_utf8 = std::str::from_utf8(&data).is_ok();
    content.text = String::from_utf8_lossy(&data).into_owned();
    Ok(content)
}

pub async fn read_file(root: &Path, path: &str) -> Result<FileContent, ApiError> {
    let file = resolve(root, path)?;
    if !file.is_file() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("{path} is not a file"),
        ));
    }
    let owned = path.to_string();
    tokio::task::spawn_blocking(move || read_text(&file, &owned))
        .await
        .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?
        .map_err(|err| io_error(path, err))
}

fn read_chunk_blocking(file: &Path, offset: u64, len: u32) -> std::io::Result<FileChunk> {
    let mut handle = std::fs::File::open(file)?;
    // The size and time come from the opened file, so they describe the bytes
    // that are read even if the path is replaced meanwhile.
    let meta = handle.metadata()?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| u64::try_from(since.as_nanos()).unwrap_or(u64::MAX));
    let mut data = Vec::new();
    if offset < meta.len() {
        let len = len.min(MAX_FILE_CHUNK) as u64;
        data.reserve(len.min(meta.len() - offset) as usize);
        handle.seek(SeekFrom::Start(offset))?;
        handle.take(len).read_to_end(&mut data)?;
    }
    Ok(FileChunk {
        size: meta.len(),
        modified,
        data,
    })
}

/// Reads up to `len` bytes of a file from `offset` on, for a download.
pub async fn read_chunk(
    root: &Path,
    path: &str,
    offset: u64,
    len: u32,
) -> Result<FileChunk, ApiError> {
    let file = resolve(root, path)?;
    if !file.is_file() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("{path} is not a file"),
        ));
    }
    tokio::task::spawn_blocking(move || read_chunk_blocking(&file, offset, len))
        .await
        .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?
        .map_err(|err| io_error(path, err))
}

/// Replaces the text of an existing file, if it still has the `expected`
/// version. Holds the file's write lock, so it never interleaves with a tool
/// call on the same file. Sessions that read the file earlier notice the
/// change and have to read it again before editing it.
pub async fn write_file(
    workspace: &Workspace,
    path: &str,
    text: String,
    expected: FileVersion,
) -> Result<FileVersion, ApiError> {
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("{path} would be larger than 2 MB"),
        ));
    }
    let file = resolve(workspace.root(), path)?;
    if !file.is_file() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("{path} is not a file"),
        ));
    }
    let _guard = workspace.locks().lock_file(&file, FileAccess::Write).await;
    let owned = path.to_string();
    tokio::task::spawn_blocking(move || {
        // A file over the read limit cannot have a version from a full read,
        // so there is no need to hash all of it.
        let mut current = Vec::new();
        std::fs::File::open(&file)
            .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut current))
            .map_err(|err| io_error(&owned, err))?;
        if current.len() as u64 > MAX_FILE_BYTES || version(&current) != expected {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!("{owned} changed since it was read"),
            ));
        }
        loom_tools::write_file(&file, text.as_bytes()).map_err(|err| io_error(&owned, err))?;
        Ok(version(text.as_bytes()))
    })
    .await
    .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?
}
