use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::frame::{Diagnostics, FrameStream, PeerInfo};

struct ChildGuard(Mutex<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let child = self.0.get_mut().unwrap_or_else(|e| e.into_inner());
        let _ = child.start_kill();
    }
}

pub fn connect_command(mut command: Command) -> io::Result<FrameStream> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no stdout"))?;
    let diagnostics = Diagnostics::default();
    if let Some(mut stderr) = child.stderr.take() {
        let sink = diagnostics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                sink.push(&String::from_utf8_lossy(&buf[..n]));
            }
        });
    }
    let guard = Arc::new(ChildGuard(Mutex::new(child)));
    Ok(FrameStream::with_guard(
        stdout,
        stdin,
        PeerInfo::Command { program },
        Some(guard),
        Some(diagnostics),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub destination: String,
    pub remote_bin: String,
    pub control_dir: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

impl SshTarget {
    pub const DEFAULT_REMOTE_BIN: &str = "loom-agent";

    pub fn new(destination: impl Into<String>) -> Self {
        Self {
            destination: destination.into(),
            remote_bin: Self::DEFAULT_REMOTE_BIN.into(),
            control_dir: None,
            extra_args: Vec::new(),
        }
    }

    pub fn args(&self, remote_args: &[String]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["-T".into()];
        if let Some(dir) = &self.control_dir {
            args.extend(
                [
                    "-o".into(),
                    "ControlMaster=auto".into(),
                    "-o".into(),
                    control_path(dir),
                    "-o".into(),
                    "ControlPersist=600".into(),
                ]
                .into_iter()
                .map(OsString::from),
            );
        }
        args.extend(self.extra_args.iter().map(OsString::from));
        args.push(self.destination.clone().into());
        args.push("--".into());
        let remote: Vec<String> = std::iter::once(self.remote_bin.as_str())
            .chain(remote_args.iter().map(String::as_str))
            .map(shell_quote)
            .collect();
        args.push(remote.join(" ").into());
        args
    }

    pub fn command(&self, remote_args: &[String]) -> Command {
        let mut command = Command::new("ssh");
        command.args(self.args(remote_args));
        command
    }
}

fn control_path(dir: &Path) -> String {
    format!("ControlPath={}", dir.join("%C").display())
}

pub fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b));
    if safe {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_remote_arguments() {
        assert_eq!(shell_quote("stdio"), "stdio");
        assert_eq!(shell_quote("~/src/my repo"), "'~/src/my repo'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    #[test]
    fn builds_ssh_arguments() {
        let mut target = SshTarget::new("me@dev-box");
        target.control_dir = Some(PathBuf::from("/run/user/1/loom/ssh"));
        let args: Vec<String> = target
            .args(&["stdio".into(), "--workspace".into(), "/src/a b".into()])
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(args[0], "-T");
        assert!(args.contains(&"ControlPath=/run/user/1/loom/ssh/%C".to_owned()));
        let dest = args.iter().position(|a| a == "me@dev-box").unwrap();
        assert_eq!(args[dest + 1], "--");
        assert_eq!(args[dest + 2], "loom-agent stdio --workspace '/src/a b'");
    }
}
