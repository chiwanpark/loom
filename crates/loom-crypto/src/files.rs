use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(crate) struct FileLock {
    _file: File,
}

pub(crate) fn lock(path: &Path) -> io::Result<FileLock> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let lock_path = lock_path(path);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)?;
    file.lock()?;
    Ok(FileLock { _file: file })
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;
    let mut tmp = tempfile_in(dir, path)?;
    tmp.1.write_all(bytes)?;
    tmp.1.sync_all()?;
    std::fs::rename(&tmp.0, path)?;
    Ok(())
}

fn tempfile_in(dir: &Path, target: &Path) -> io::Result<(PathBuf, File)> {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    for attempt in 0..100u32 {
        let candidate = dir.join(format!(".{name}.{}.{attempt}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::other("could not create a temporary file"))
}
