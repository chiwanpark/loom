use std::ffi::OsString;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    path_beneath_rules,
};

pub const SANDBOX_SUBCOMMAND: &str = "sandbox-exec";
const ABI_VERSION: ABI = ABI::V5;
const DEFAULT_WRITABLE: &[&str] = &["/dev", "/tmp", "/var/tmp"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxPolicy {
    pub writable: Vec<PathBuf>,
    pub network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherKind {
    Landlock { exe: PathBuf },
    Bubblewrap { bwrap: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLauncher {
    pub kind: LauncherKind,
    pub policy: SandboxPolicy,
}

impl SandboxLauncher {
    pub fn argv(&self, command: &[OsString]) -> (PathBuf, Vec<OsString>) {
        match &self.kind {
            LauncherKind::Landlock { exe } => {
                let mut args: Vec<OsString> = vec![SANDBOX_SUBCOMMAND.into()];
                for path in &self.policy.writable {
                    args.push("--write".into());
                    args.push(path.as_os_str().to_owned());
                }
                if !self.policy.network {
                    args.push("--no-network".into());
                }
                args.push("--".into());
                args.extend(command.iter().cloned());
                (exe.clone(), args)
            }
            LauncherKind::Bubblewrap { bwrap } => {
                let mut args: Vec<OsString> =
                    ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
                        .into_iter()
                        .map(OsString::from)
                        .collect();
                let mut writable: Vec<PathBuf> =
                    DEFAULT_WRITABLE[1..].iter().map(PathBuf::from).collect();
                writable.extend(self.policy.writable.iter().cloned());
                for path in existing(writable) {
                    args.push("--bind".into());
                    args.push(path.as_os_str().to_owned());
                    args.push(path.into_os_string());
                }
                if !self.policy.network {
                    args.push("--unshare-net".into());
                }
                args.extend(["--die-with-parent", "--"].into_iter().map(OsString::from));
                args.extend(command.iter().cloned());
                (bwrap.clone(), args)
            }
        }
    }
}

pub fn find_bwrap() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_file())
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("usage: sandbox-exec [--write PATH]... [--no-network] -- PROGRAM [ARGS]...")]
    Usage,
    #[error("landlock is not available on this system")]
    Unsupported,
    #[error("landlock: {0}")]
    Landlock(String),
}

pub fn parse_args(args: &[OsString]) -> Result<(SandboxPolicy, Vec<OsString>), SandboxError> {
    let mut policy = SandboxPolicy {
        writable: Vec::new(),
        network: true,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--write") => {
                let path = iter.next().ok_or(SandboxError::Usage)?;
                policy.writable.push(PathBuf::from(path));
            }
            Some("--no-network") => policy.network = false,
            Some("--") => {
                let command: Vec<OsString> = iter.cloned().collect();
                if command.is_empty() {
                    return Err(SandboxError::Usage);
                }
                return Ok((policy, command));
            }
            _ => return Err(SandboxError::Usage),
        }
    }
    Err(SandboxError::Usage)
}

fn existing(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().filter(|p| p.exists()).collect()
}

pub fn enforce(policy: &SandboxPolicy) -> Result<RulesetStatus, SandboxError> {
    let err = |e: landlock::RulesetError| SandboxError::Landlock(e.to_string());
    let mut writable = existing(policy.writable.iter().cloned());
    writable.extend(existing(DEFAULT_WRITABLE.iter().map(PathBuf::from)));
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        writable.extend(existing([PathBuf::from(tmp)]));
    }
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI_VERSION))
        .map_err(err)?;
    if !policy.network {
        ruleset = ruleset
            .handle_access(AccessNet::from_all(ABI_VERSION))
            .map_err(err)?;
    }
    let status = ruleset
        .create()
        .map_err(err)?
        .add_rules(path_beneath_rules(
            [Path::new("/")],
            AccessFs::from_read(ABI_VERSION),
        ))
        .map_err(err)?
        .add_rules(path_beneath_rules(
            &writable,
            AccessFs::from_all(ABI_VERSION),
        ))
        .map_err(err)?
        .restrict_self()
        .map_err(err)?;
    match status.ruleset {
        RulesetStatus::NotEnforced => Err(SandboxError::Unsupported),
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_arguments_round_trip() {
        let launcher = SandboxLauncher {
            kind: LauncherKind::Landlock {
                exe: PathBuf::from("/usr/bin/loom-agent"),
            },
            policy: SandboxPolicy {
                writable: vec![PathBuf::from("/w s")],
                network: false,
            },
        };
        let (program, args) = launcher.argv(&["/bin/sh".into(), "-c".into(), "echo hi".into()]);
        assert_eq!(program, PathBuf::from("/usr/bin/loom-agent"));
        assert_eq!(args[0], SANDBOX_SUBCOMMAND);
        let (policy, command) = parse_args(&args[1..]).unwrap();
        assert_eq!(policy, launcher.policy);
        assert_eq!(command, ["/bin/sh", "-c", "echo hi"]);
        assert!(parse_args(&["--write".into()]).is_err());
        assert!(parse_args(&["--".into()]).is_err());
        assert!(parse_args(&["--bogus".into()]).is_err());

        let dir = tempfile::tempdir().unwrap();
        let bwrap = SandboxLauncher {
            kind: LauncherKind::Bubblewrap {
                bwrap: PathBuf::from("/usr/bin/bwrap"),
            },
            policy: SandboxPolicy {
                writable: vec![dir.path().to_path_buf(), PathBuf::from("/missing/dir")],
                network: false,
            },
        };
        let (program, args) = bwrap.argv(&["/bin/true".into()]);
        assert_eq!(program, PathBuf::from("/usr/bin/bwrap"));
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            text.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == dir.path().to_string_lossy())
        );
        assert!(!text.iter().any(|a| a == "/missing/dir"));
        assert!(text.contains(&"--unshare-net".to_owned()));
        assert_eq!(text.last().unwrap(), "/bin/true");
    }

    #[tokio::test]
    async fn bubblewrap_confines_writes() {
        let Some(bwrap) = find_bwrap() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
        let launcher = SandboxLauncher {
            kind: LauncherKind::Bubblewrap { bwrap },
            policy: SandboxPolicy {
                writable: vec![dir.path().to_path_buf()],
                network: true,
            },
        };
        let script = format!(
            "echo ok > {}/inside && echo bad > {}/outside",
            dir.path().display(),
            outside.path().display()
        );
        let (program, args) = launcher.argv(&["/bin/sh".into(), "-c".into(), script.into()]);
        let status = tokio::process::Command::new(program)
            .args(args)
            .status()
            .await
            .unwrap();
        assert!(!status.success());
        assert!(dir.path().join("inside").exists());
        assert!(!outside.path().join("outside").exists());
    }
}
