use std::path::{Path, PathBuf};

use loom_client::oracle_proto::{
    CheckResult, InstanceVolume, VOLUME_PREFIX, VolumeKind, check_mount_path, nested,
    valid_absolute_path, valid_name,
};
use loom_config::{HostRules, HostUser};

pub const DEV_UID: u32 = 1000;
pub const DEV_GID: u32 = 1000;

pub fn incus_volume(name: &str) -> anyhow::Result<String> {
    if !valid_name(name) {
        anyhow::bail!("{name:?} is not a volume name");
    }
    Ok(format!("{VOLUME_PREFIX}{name}"))
}

pub fn idmap(user: HostUser) -> String {
    format!("uid {} {DEV_UID}\ngid {} {DEV_GID}", user.uid, user.gid)
}

pub fn host_directory(rules: &HostRules, path: &str) -> Result<PathBuf, String> {
    if !valid_absolute_path(path) || path == "/" {
        return Err(format!("{path:?} is not a normalized directory path"));
    }
    let path = Path::new(path);
    let Some(root) = rules.directories.iter().find(|root| path.starts_with(root)) else {
        return Err(format!(
            "{} is not in rules.directories of host.yaml",
            path.display()
        ));
    };
    let real = std::fs::canonicalize(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let real_root =
        std::fs::canonicalize(root).map_err(|err| format!("{}: {err}", root.display()))?;
    if real_root != *root || !real.starts_with(root) {
        return Err(format!(
            "{} leads out of {} through a link",
            path.display(),
            root.display()
        ));
    }
    if !real.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    Ok(real)
}

pub fn check_directories(
    rules: &HostRules,
    local: bool,
    directories: &[String],
) -> Vec<CheckResult> {
    directories
        .iter()
        .map(|directory| CheckResult {
            name: directory.clone(),
            error: mount_source(rules, local, directory).err(),
        })
        .collect()
}

fn mount_source(rules: &HostRules, local: bool, path: &str) -> Result<PathBuf, String> {
    if rules.host_user.is_none() {
        return Err("set rules.host_user in host.yaml to mount host directories".into());
    }
    if local {
        host_directory(rules, path)
    } else {
        remote_directory(rules, path)
    }
}

fn remote_directory(rules: &HostRules, path: &str) -> Result<PathBuf, String> {
    if !valid_absolute_path(path) || path == "/" {
        return Err(format!("{path:?} is not a normalized directory path"));
    }
    let path = Path::new(path);
    if !rules.directories.iter().any(|root| path.starts_with(root)) {
        return Err(format!(
            "{} is not in rules.directories of host.yaml",
            path.display()
        ));
    }
    Ok(path.to_path_buf())
}

pub fn prepare_mounts(
    rules: &HostRules,
    local: bool,
    volumes: &[InstanceVolume],
) -> anyhow::Result<Vec<InstanceVolume>> {
    let mut mounts = Vec::with_capacity(volumes.len());
    for (index, volume) in volumes.iter().enumerate() {
        incus_volume(&volume.name)?;
        check_mount_path(&volume.path, &[]).map_err(anyhow::Error::msg)?;
        if volumes[..index]
            .iter()
            .any(|other| other.name == volume.name || nested(&other.path, &volume.path))
        {
            anyhow::bail!(
                "the volume {} duplicates a name or overlaps a mount path",
                volume.name
            );
        }
        let mut volume = volume.clone();
        match (volume.kind, &volume.source) {
            (VolumeKind::Incus, None) => {}
            (VolumeKind::Directory, Some(source)) if volume.size_mib.is_none() => {
                let real = mount_source(rules, local, source)
                    .map_err(|err| anyhow::anyhow!("the volume {}: {err}", volume.name))?;
                let path = real.into_os_string().into_string().map_err(|_| {
                    anyhow::anyhow!("the volume {} must have a UTF-8 source path", volume.name)
                })?;
                volume.source = Some(path);
            }
            _ => anyhow::bail!(
                "the volume {} has an invalid kind, size or source",
                volume.name
            ),
        }
        mounts.push(volume);
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_directories_stay_in_the_allowed_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let allowed = root.join("allowed");
        std::fs::create_dir_all(allowed.join("project")).unwrap();
        std::fs::create_dir_all(root.join("secret")).unwrap();
        std::os::unix::fs::symlink(root.join("secret"), allowed.join("escape")).unwrap();
        std::fs::write(allowed.join("file"), "x").unwrap();
        let rules = HostRules {
            directories: vec![allowed.clone()],
            host_user: Some(HostUser {
                uid: 1001,
                gid: 1002,
            }),
            ..HostRules::default()
        };
        let path = |rest: &str| root.join(rest).to_string_lossy().into_owned();
        assert_eq!(
            host_directory(&rules, &path("allowed/project")).unwrap(),
            allowed.join("project")
        );
        assert!(host_directory(&rules, &path("allowed")).is_ok());
        for bad in [
            "secret",
            "allowed/escape",
            "allowed/file",
            "allowed/none",
            "allowed/../secret",
        ] {
            assert!(host_directory(&rules, &path(bad)).is_err(), "{bad}");
        }
        assert!(host_directory(&rules, &path("allowedx")).is_err());
        let remote = HostRules {
            directories: vec![PathBuf::from("/srv/data")],
            ..rules.clone()
        };
        let checked = check_directories(
            &remote,
            false,
            &["/srv/data/team".into(), "/srv/data".into()],
        );
        assert!(checked.iter().all(|item| item.error.is_none()));
        for bad in ["/srv/datax", "/srv", "/srv/data/../x", "/srv/data/", "data"] {
            let checked = check_directories(&remote, false, &[bad.into()]);
            assert!(checked[0].error.is_some(), "{bad}");
        }
        let remote_no_user = HostRules {
            host_user: None,
            ..remote.clone()
        };
        assert!(
            check_directories(&remote_no_user, false, &["/srv/data".into()])[0]
                .error
                .is_some()
        );
        let no_user = HostRules {
            host_user: None,
            ..rules.clone()
        };
        let checked = check_directories(&no_user, true, &[path("allowed/project")]);
        assert!(checked[0].error.as_deref().unwrap().contains("host_user"));
        assert_eq!(
            idmap(rules.host_user.unwrap()),
            "uid 1001 1000\ngid 1002 1000"
        );
        assert_eq!(incus_volume("cache").unwrap(), "loom-cache");
        assert!(incus_volume("../x").is_err());
    }
}
