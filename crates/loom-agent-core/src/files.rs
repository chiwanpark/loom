use std::io::Read;
use std::path::{Component, Path, PathBuf};

use loom_proto::{ApiError, DirEntry, DirectoryListing, EntryKind, ErrorCode, FileContent};

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

fn read_text(file: &Path) -> std::io::Result<(String, u64, bool, bool)> {
    let size = std::fs::metadata(file)?.len();
    let mut data = Vec::new();
    std::fs::File::open(file)?
        .take(MAX_FILE_BYTES)
        .read_to_end(&mut data)?;
    if data[..data.len().min(BINARY_SNIFF)].contains(&0) {
        return Ok((String::new(), size, true, false));
    }
    let truncated = size > data.len() as u64;
    if truncated && let Some(end) = data.iter().rposition(|b| *b == b'\n') {
        data.truncate(end + 1);
    }
    Ok((
        String::from_utf8_lossy(&data).into_owned(),
        size,
        false,
        truncated,
    ))
}

pub async fn read_file(root: &Path, path: &str) -> Result<FileContent, ApiError> {
    let file = resolve(root, path)?;
    if !file.is_file() {
        return Err(ApiError::new(
            ErrorCode::BadRequest,
            format!("{path} is not a file"),
        ));
    }
    let (text, size, binary, truncated) = tokio::task::spawn_blocking(move || read_text(&file))
        .await
        .map_err(|err| ApiError::new(ErrorCode::Internal, err.to_string()))?
        .map_err(|err| io_error(path, err))?;
    Ok(FileContent {
        path: path.to_string(),
        text,
        size,
        binary,
        truncated,
    })
}
