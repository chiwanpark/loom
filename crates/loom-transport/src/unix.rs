use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use crate::frame::{FrameStream, PeerInfo};

const MAX_SOCKET_PATH: usize = 107;

pub fn current_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

fn check_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is longer than {MAX_SOCKET_PATH} bytes",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn stream(socket: UnixStream) -> io::Result<FrameStream> {
    let cred = socket.peer_cred()?;
    let peer = PeerInfo::Local {
        uid: cred.uid(),
        pid: cred.pid(),
    };
    let (read, write) = socket.into_split();
    Ok(FrameStream::new(read, write, peer))
}

pub async fn connect_unix(path: &Path) -> io::Result<FrameStream> {
    check_path(path)?;
    let socket = UnixStream::connect(path).await?;
    let stream = stream(socket)?;
    if let PeerInfo::Local { uid, .. } = stream.peer()
        && *uid != current_uid()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by another user", path.display()),
        ));
    }
    Ok(stream)
}

pub struct UnixFrameListener {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
}

impl UnixFrameListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        check_path(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            uid: current_uid(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn accept(&self) -> io::Result<FrameStream> {
        loop {
            let (socket, _) = self.listener.accept().await?;
            let stream = match stream(socket) {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!(error = %err, "could not read peer credentials");
                    continue;
                }
            };
            match stream.peer() {
                PeerInfo::Local { uid, .. } if *uid == self.uid => return Ok(stream),
                peer => tracing::warn!(?peer, "rejected a connection from another user"),
            }
        }
    }
}

impl Drop for UnixFrameListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
