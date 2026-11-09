use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use crate::workspace::FileStamp;

pub(crate) fn current_stamp(path: &Path) -> io::Result<Option<FileStamp>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(FileStamp::from_metadata(&metadata))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub(crate) fn write_file(path: &Path, content: &[u8]) -> io::Result<(FileStamp, bool)> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    "path is a directory",
                ));
            }
            replace_atomically(path, content, metadata.permissions())?;
            Ok((FileStamp::from_metadata(&fs::metadata(path)?), false))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(content)?;
            file.sync_all()?;
            Ok((FileStamp::from_metadata(&file.metadata()?), true))
        }
        Err(err) => Err(err),
    }
}

fn replace_atomically(path: &Path, content: &[u8], permissions: fs::Permissions) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".loom-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    fs::set_permissions(temp.path(), permissions)?;
    temp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        let (_, created) = write_file(&path, b"hi").unwrap();
        assert!(created);
        assert_eq!(fs::read(&path).unwrap(), b"hi");
    }

    #[test]
    fn preserves_permissions_on_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let (_, created) = write_file(&path, b"new").unwrap();
        assert!(!created);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn refuses_to_follow_dangling_symlinks() {
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(outside.path().join("target"), &link).unwrap();
        assert!(write_file(&link, b"x").is_err());
        assert!(!outside.path().join("target").exists());
    }
}
