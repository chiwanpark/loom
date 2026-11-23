use std::fmt;

use serde::{Deserialize, Serialize};

pub const VOLUME_PREFIX: &str = "loom-";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeKind {
    #[default]
    Incus,
    Directory,
    #[serde(other)]
    Unknown,
}

impl VolumeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incus => "incus",
            Self::Directory => "directory",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "incus" => Self::Incus,
            "directory" => Self::Directory,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for VolumeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    pub kind: VolumeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub deleting: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInput {
    pub name: String,
    #[serde(default)]
    pub kind: VolumeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVolume {
    pub volume: String,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceVolume {
    pub name: String,
    pub kind: VolumeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volumes {
    pub volumes: Vec<Volume>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeResult {
    pub volume: Volume,
}

const SYSTEM_DIRECTORIES: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib32", "/lib64", "/libx32", "/proc", "/root",
    "/run", "/sbin", "/sys", "/tmp", "/usr", "/var",
];
const LOOM_DIRECTORIES: &[&str] = &[
    "/home/dev/.config/loom",
    "/home/dev/.local/share/loom",
    "/home/dev/.local/state/loom",
];
pub const WORKSPACES_ROOT: &str = "/workspaces";

pub fn nested(a: &str, b: &str) -> bool {
    let within = |inner: &str, outer: &str| {
        inner == outer
            || outer == "/"
            || inner
                .strip_prefix(outer)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    within(a, b) || within(b, a)
}

pub fn clone_paths(workspace: &str, directories: &[String]) -> Vec<String> {
    match directories {
        [one] => vec![format!("{WORKSPACES_ROOT}/{one}")],
        many => many
            .iter()
            .map(|directory| format!("{WORKSPACES_ROOT}/{workspace}/{directory}"))
            .collect(),
    }
}

pub fn valid_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 200
        && (path == "/" || !path.ends_with('/'))
        && !path.contains("//")
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        && !path.split('/').any(|part| part == "." || part == "..")
}

pub fn check_mount_path(path: &str, clones: &[String]) -> Result<(), String> {
    if !valid_absolute_path(path) {
        return Err(format!(
            "{path:?} is not a mount path: use an absolute path with letters, digits, '.', '_', '-', and '/'"
        ));
    }
    if path == "/" || path == WORKSPACES_ROOT {
        return Err(format!("a volume cannot replace {path}"));
    }
    if let Some(system) = SYSTEM_DIRECTORIES.iter().find(|dir| nested(path, dir)) {
        return Err(format!("{path} is the system directory {system} or in it"));
    }
    if let Some(loom) = LOOM_DIRECTORIES.iter().find(|dir| nested(path, dir)) {
        return Err(format!("{path} hides or is in the Loom directory {loom}"));
    }
    if let Some(clone) = clones.iter().find(|clone| nested(path, clone)) {
        return Err(format!("{path} hides or is in the clone {clone}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_paths_keep_away_from_the_system_and_the_clones() {
        let one = clone_paths("acme", &["server".into()]);
        assert_eq!(one, vec!["/workspaces/server".to_owned()]);
        let two = clone_paths("acme", &["server".into(), "client".into()]);
        assert_eq!(two[1], "/workspaces/acme/client");
        for good in [
            "/cache/build",
            "/workspaces/data",
            "/workspaces/acme/data",
            "/home/dev/.cache",
            "/mnt/x",
            "/opt/tools",
        ] {
            assert_eq!(check_mount_path(good, &two), Ok(()), "{good}");
        }
        for bad in [
            "/",
            "/workspaces",
            "/workspaces/acme",
            "/workspaces/acme/client",
            "/workspaces/acme/client/target",
            "/usr/local",
            "/var",
            "/home",
            "/home/dev",
            "/home/dev/.config",
            "/home/dev/.local/share/loom/x",
            "relative",
            "/a/../b",
            "/a//b",
            "/a/",
            "/a b",
        ] {
            assert!(check_mount_path(bad, &two).is_err(), "{bad}");
        }
        assert_eq!(check_mount_path("/workspaces/acme", &one), Ok(()));
        assert!(check_mount_path("/workspaces/server/x", &one).is_err());
    }

    #[test]
    fn nested_paths_share_a_prefix_of_whole_components() {
        assert!(nested("/a", "/a/b"));
        assert!(nested("/a/b", "/a"));
        assert!(nested("/a", "/a"));
        assert!(!nested("/a", "/ab"));
        assert!(nested("/", "/x"));
    }
}
