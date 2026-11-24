use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use loom_client::oracle_proto::{
    ARCHIVE_CHUNK, ArchiveChunk, ArchiveFile, ArchiveFiles, ErrorCode, OracleError,
    valid_archive_file,
};
use sha2::{Digest, Sha256};

use crate::driver::Driver;

pub const ARCHIVE_DIR: &str = "archive";
const MAX_SIZE: u64 = 16 << 30;

fn error(code: ErrorCode, message: impl Into<String>) -> OracleError {
    OracleError::new(code, message)
}

fn internal(message: impl Into<String>) -> OracleError {
    error(ErrorCode::Internal, message)
}

fn codespace_dir(root: &Path, codespace: &str) -> Result<PathBuf, OracleError> {
    let valid = !codespace.is_empty()
        && codespace.len() <= 64
        && codespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid {
        return Err(error(
            ErrorCode::BadRequest,
            format!("{codespace:?} is not a codespace ID"),
        ));
    }
    Ok(root.join(codespace))
}

fn describe(dir: &Path) -> std::io::Result<Vec<ArchiveFile>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_archive_file(&name) {
            continue;
        }
        let mut file = std::fs::File::open(entry.path())?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 64 * 1024];
        let mut size = 0;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
            size += read as u64;
        }
        let sha256 = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        files.push(ArchiveFile { name, size, sha256 });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

pub async fn files(
    driver: &dyn Driver,
    oracle: &str,
    root: &Path,
    codespace: &str,
    instance: &str,
    max_size: Option<u64>,
) -> Result<ArchiveFiles, OracleError> {
    let dir = codespace_dir(root, codespace)?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|err| internal(format!("cannot make {}: {err}", dir.display())))?;
    let max_size = max_size.unwrap_or(MAX_SIZE).min(MAX_SIZE);
    let copied = driver
        .copy_sessions(oracle, instance, &dir, max_size)
        .await
        .map_err(|err| internal(format!("{err:#}")));
    let described = copied.and_then(|()| {
        describe(&dir).map_err(|err| internal(format!("cannot read the copies: {err}")))
    });
    match described {
        Ok(files) => {
            tracing::info!(%codespace, files = files.len(), "copied the sessions of a codespace");
            Ok(ArchiveFiles { files })
        }
        Err(err) => {
            remove(root, codespace);
            Err(err)
        }
    }
}

pub fn chunk(
    root: &Path,
    codespace: &str,
    name: &str,
    offset: u64,
) -> Result<ArchiveChunk, OracleError> {
    let dir = codespace_dir(root, codespace)?;
    if !valid_archive_file(name) {
        return Err(error(
            ErrorCode::BadRequest,
            format!("{name:?} is not a session file"),
        ));
    }
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(dir.join(name))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::with_capacity(ARCHIVE_CHUNK);
        file.take(ARCHIVE_CHUNK as u64).read_to_end(&mut data)?;
        Ok(data)
    };
    match read() {
        Ok(data) => Ok(ArchiveChunk {
            data: STANDARD.encode(data),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(error(
            ErrorCode::NotFound,
            format!("the host has no copy of {name} for {codespace}"),
        )),
        Err(err) => Err(internal(format!("cannot read the copy of {name}: {err}"))),
    }
}

pub fn remove(root: &Path, codespace: &str) {
    let Ok(dir) = codespace_dir(root, codespace) else {
        return;
    };
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(%codespace, error = %err, "cannot remove the copies of the sessions");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_read_the_copies_in_parts() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("k3x9");
        std::fs::create_dir_all(&dir).unwrap();
        let content: Vec<u8> = (0..ARCHIVE_CHUNK + 10).map(|i| i as u8).collect();
        std::fs::write(dir.join("0a.sqlite"), &content).unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let files = describe(&dir).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size, content.len() as u64);

        let first = chunk(root.path(), "k3x9", "0a.sqlite", 0).unwrap();
        let second = chunk(root.path(), "k3x9", "0a.sqlite", ARCHIVE_CHUNK as u64).unwrap();
        let mut joined = STANDARD.decode(first.data).unwrap();
        joined.extend(STANDARD.decode(second.data).unwrap());
        assert_eq!(joined, content);

        assert!(chunk(root.path(), "../etc", "0a.sqlite", 0).is_err());
        assert!(chunk(root.path(), "k3x9", "../x.sqlite", 0).is_err());
        assert_eq!(
            chunk(root.path(), "k3x9", "0b.sqlite", 0).unwrap_err().code,
            ErrorCode::NotFound
        );
        remove(root.path(), "k3x9");
        assert!(!dir.exists());
    }
}
