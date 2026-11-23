use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use loom_client::oracle_proto::{
    Bootstrap, CreateInstance, InstanceReport, InstanceVolume, VolumeKind, WORKSPACES_ROOT,
};
use loom_config::HostRules;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};

use crate::driver::{
    Agent, CheckItem, CheckStatus, Driver, DriverReport, Progress, cpus, memory_mib, valid_instance,
};
use crate::volumes::prepare_mounts;

const INSTANCE_FILE: &str = "instance.json";
const LOG_FILE: &str = "agent.log";
const VOLUMES_DIR: &str = "volumes";
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const PASSED_ENV: &[&str] = &["PATH", "LANG", "LC_ALL", "TZ", "LOOM_LOG", "SSL_CERT_FILE"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstanceFile {
    codespace: String,
    instance: String,
    oracle: String,
    pid: i32,
    #[serde(default)]
    volumes: Vec<String>,
}

pub struct ProcessDriver {
    root: PathBuf,
    rules: HostRules,
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

impl ProcessDriver {
    pub fn new(root: PathBuf, rules: HostRules) -> Self {
        Self { root, rules }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, instance: &str) -> PathBuf {
        self.root.join(instance)
    }

    fn volume_dir(&self, oracle: &str, volume: &str) -> anyhow::Result<PathBuf> {
        let name = crate::volumes::incus_volume(volume)?;
        if oracle.is_empty()
            || !oracle
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("{oracle:?} is not an oracle ID");
        }
        Ok(self.root.join(VOLUMES_DIR).join(oracle).join(name))
    }

    fn link_volumes(
        &self,
        dir: &Path,
        oracle: &str,
        volumes: &[InstanceVolume],
    ) -> anyhow::Result<()> {
        for volume in volumes {
            let relative = volume
                .path
                .strip_prefix(WORKSPACES_ROOT)
                .and_then(|rest| rest.strip_prefix('/'))
                .filter(|rest| !rest.is_empty())
                .with_context(|| {
                    format!(
                        "the process driver mounts volumes only below {WORKSPACES_ROOT}, not at {}",
                        volume.path
                    )
                })?;
            let target = match (volume.kind, &volume.source) {
                (VolumeKind::Directory, Some(source)) => PathBuf::from(source),
                (VolumeKind::Incus, None) => {
                    let target = self.volume_dir(oracle, &volume.name)?;
                    std::fs::create_dir_all(&target)
                        .with_context(|| format!("cannot make {}", target.display()))?;
                    target.canonicalize()?
                }
                _ => bail!("the volume {} has an unknown kind", volume.name),
            };
            let link = dir.join("workspaces").join(relative);
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot make {}", parent.display()))?;
            }
            std::os::unix::fs::symlink(&target, &link)
                .with_context(|| format!("cannot link {}", link.display()))?;
        }
        Ok(())
    }

    fn read(&self, instance: &str) -> Option<InstanceFile> {
        let bytes = std::fs::read(self.dir(instance).join(INSTANCE_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn is_ours(&self, file: &InstanceFile) -> bool {
        let cwd = std::fs::read_link(format!("/proc/{}/cwd", file.pid)).ok();
        cwd.is_some_and(|cwd| cwd == self.dir(&file.instance))
    }

    fn running(&self, file: &InstanceFile) -> bool {
        kill(Pid::from_raw(file.pid), None).is_ok() && self.is_ours(file)
    }

    async fn stop(&self, file: &InstanceFile) {
        if !self.running(file) {
            return;
        }
        let pid = Pid::from_raw(file.pid);
        let _ = kill(pid, Signal::SIGTERM);
        let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if !self.running(file) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tracing::warn!(instance = %file.instance, "the agent did not stop; killing it");
        let _ = kill(pid, Signal::SIGKILL);
    }
}

#[async_trait]
impl Driver for ProcessDriver {
    async fn report(&self) -> anyhow::Result<DriverReport> {
        Ok(DriverReport {
            driver: "process".into(),
            cpus: cpus(),
            memory_mib: memory_mib(),
            architecture: Some(std::env::consts::ARCH.into()),
            storage_driver: None,
            incus_version: None,
        })
    }

    async fn create(
        &self,
        request: &CreateInstance,
        oracle: &str,
        agent: &Agent,
        progress: &Progress,
    ) -> anyhow::Result<()> {
        if !valid_instance(&request.instance) {
            bail!("{:?} is not an instance name of Loom", request.instance);
        }
        let dir = self.dir(&request.instance);
        if dir.exists() {
            bail!("the instance {} exists already", request.instance);
        }
        let mounts = prepare_mounts(&self.rules, true, &request.volumes)?;
        progress.step("instance");
        let home = dir.join("home");
        let config = home.join(".config/loom");
        let workspaces = dir.join("workspaces");
        let runtime = dir.join("run");
        for path in [&config, &workspaces, &runtime] {
            std::fs::create_dir_all(path)
                .with_context(|| format!("cannot make {}", path.display()))?;
        }
        self.link_volumes(&dir, oracle, &mounts)?;
        let bootstrap = Bootstrap {
            workspaces_root: Some(workspaces),
            ..request.bootstrap.clone()
        };
        write_private(
            &config.join("bootstrap.json"),
            &serde_json::to_vec_pretty(&bootstrap)?,
        )
        .context("cannot write the bootstrap file")?;
        let log = std::fs::File::create(dir.join(LOG_FILE)).context("cannot make the log file")?;
        let mut command = tokio::process::Command::new(&agent.path);
        command
            .args(["serve", "--managed"])
            .current_dir(&dir)
            .env_clear()
            .envs(
                PASSED_ENV
                    .iter()
                    .filter_map(|name| Some((*name, std::env::var_os(name)?))),
            )
            .envs(&request.environment.variables)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .kill_on_drop(false);
        progress.step("start");
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {}", agent.path.display()))?;
        let pid = child.id().context("the agent stopped at once")?;
        let file = InstanceFile {
            codespace: request.codespace.clone(),
            instance: request.instance.clone(),
            oracle: oracle.to_owned(),
            pid: i32::try_from(pid)?,
            volumes: mounts
                .into_iter()
                .filter(|mount| mount.kind == VolumeKind::Incus)
                .map(|mount| mount.name)
                .collect(),
        };
        write_private(&dir.join(INSTANCE_FILE), &serde_json::to_vec_pretty(&file)?)?;
        let instance = request.instance.clone();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => tracing::info!(%instance, %status, "a codespace agent stopped"),
                Err(err) => tracing::warn!(%instance, error = %err, "cannot wait for an agent"),
            }
        });
        Ok(())
    }

    async fn delete(&self, oracle: &str, instance: &str) -> anyhow::Result<()> {
        if !valid_instance(instance) {
            bail!("{instance:?} is not an instance name of Loom");
        }
        let dir = self.dir(instance);
        if !dir.exists() {
            return Ok(());
        }
        match self.read(instance) {
            Some(file) if file.oracle != oracle => {
                bail!("the instance {instance} belongs to a different oracle")
            }
            Some(file) => self.stop(&file).await,
            None => {}
        }
        tokio::fs::remove_dir_all(&dir)
            .await
            .with_context(|| format!("cannot remove {}", dir.display()))
    }

    async fn list(&self, oracle: &str) -> anyhow::Result<Vec<InstanceReport>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.root.display()));
            }
        };
        let mut instances = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(file) = self.read(&name) else {
                continue;
            };
            if file.oracle != oracle || file.instance != name {
                continue;
            }
            instances.push(InstanceReport {
                running: self.running(&file),
                codespace: file.codespace,
                instance: file.instance,
                oracle: file.oracle,
            });
        }
        instances.sort_by(|a, b| a.instance.cmp(&b.instance));
        Ok(instances)
    }

    async fn delete_volume(&self, oracle: &str, volume: &str) -> anyhow::Result<()> {
        let target = self.volume_dir(oracle, volume)?;
        for instance in self.list(oracle).await? {
            if self
                .read(&instance.instance)
                .is_some_and(|file| file.volumes.iter().any(|name| name == volume))
            {
                bail!("the volume {volume} is in use by {}", instance.instance);
            }
        }
        match std::fs::remove_dir_all(&target) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot remove {}", target.display())),
        }
    }

    async fn check(&self, _deep: bool) -> Vec<CheckItem> {
        let mut items = Vec::new();
        match std::fs::create_dir_all(&self.root) {
            Ok(()) => items.push(CheckItem::new(
                "root",
                CheckStatus::Ok,
                self.root.display().to_string(),
            )),
            Err(err) => items.push(CheckItem::new(
                "root",
                CheckStatus::Failed,
                format!("cannot make {}: {err}", self.root.display()),
            )),
        }
        items.push(CheckItem::new(
            "isolation",
            CheckStatus::Warning,
            "the process driver runs agents as this user, without a container; use it for tests and development",
        ));
        items
    }
}

#[cfg(test)]
mod tests {
    use loom_client::oracle_proto::Environment;
    use loom_config::HostUser;

    use super::*;

    fn request(id: &str) -> CreateInstance {
        CreateInstance {
            codespace: id.into(),
            instance: format!("loom-{id}"),
            cpus: 1,
            memory_mib: 512,
            environment: Environment::default(),
            bootstrap: Bootstrap {
                link: "loom+ws://127.0.0.1:1/x".into(),
                codespace: id.into(),
                workspaces_root: None,
            },
            agent_version: None,
            volumes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn processes_are_instances_of_one_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().canonicalize().unwrap().join("data");
        std::fs::create_dir_all(data.join("dataset")).unwrap();
        std::fs::write(data.join("dataset/file"), "test data").unwrap();
        std::os::unix::fs::symlink(data.join("dataset"), data.join("alias")).unwrap();
        let driver = ProcessDriver::new(
            dir.path().join("codespaces"),
            HostRules {
                directories: vec![data.clone()],
                host_user: Some(HostUser {
                    uid: 1001,
                    gid: 1002,
                }),
                ..Default::default()
            },
        );
        assert!(driver.list("o1").await.unwrap().is_empty());
        let script = dir.path().join("agent.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let agent = Agent {
            path: script,
            version: None,
        };
        let mounts = vec![
            InstanceVolume {
                name: "cache".into(),
                kind: VolumeKind::Incus,
                size_mib: None,
                source: None,
                path: "/workspaces/cache".into(),
                read_only: false,
            },
            InstanceVolume {
                name: "data".into(),
                kind: VolumeKind::Directory,
                size_mib: None,
                source: Some(data.join("alias").to_str().unwrap().to_owned()),
                path: "/workspaces/data".into(),
                read_only: true,
            },
        ];
        let mut req = request("c1");
        req.volumes = mounts.clone();
        req.volumes[0].path = "/workspaces/../escaped".into();
        assert!(
            driver
                .create(&req, "o1", &agent, &Progress::none())
                .await
                .is_err()
        );
        assert!(!driver.dir("loom-c1").exists());
        req.volumes = mounts.clone();
        driver
            .create(&req, "o1", &agent, &Progress::none())
            .await
            .unwrap();
        let workspace = driver.dir("loom-c1").join("workspaces");
        assert_eq!(
            std::fs::read_link(workspace.join("data")).unwrap(),
            data.join("dataset")
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("data/file")).unwrap(),
            "test data"
        );
        std::fs::write(workspace.join("cache/marker"), "saved").unwrap();
        let err = driver.delete_volume("o1", "cache").await.unwrap_err();
        assert!(err.to_string().contains("in use"));
        assert!(
            driver
                .create(&request("c1"), "o1", &agent, &Progress::none())
                .await
                .is_err()
        );
        let bootstrap: Bootstrap = serde_json::from_slice(
            &std::fs::read(
                dir.path()
                    .join("codespaces/loom-c1/home/.config/loom/bootstrap.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            bootstrap.workspaces_root,
            Some(dir.path().join("codespaces/loom-c1/workspaces"))
        );
        let listed = driver.list("o1").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].codespace, "c1");
        assert!(listed[0].running);
        assert!(driver.list("o2").await.unwrap().is_empty());
        assert!(driver.delete("o2", "loom-c1").await.is_err());
        assert!(driver.delete("o1", "../x").await.is_err());

        driver.delete("o1", "loom-c1").await.unwrap();
        assert!(!dir.path().join("codespaces/loom-c1").exists());
        assert!(driver.list("o1").await.unwrap().is_empty());
        driver.delete("o1", "loom-c1").await.unwrap();
        let mut req = request("c2");
        req.volumes = mounts;
        driver
            .create(&req, "o1", &agent, &Progress::none())
            .await
            .unwrap();
        let workspace = driver.dir("loom-c2").join("workspaces");
        assert_eq!(
            std::fs::read_to_string(workspace.join("cache/marker")).unwrap(),
            "saved"
        );
        driver.delete("o1", "loom-c2").await.unwrap();
        driver.delete_volume("o1", "cache").await.unwrap();
        assert!(!driver.volume_dir("o1", "cache").unwrap().exists());
        assert!(data.join("dataset/file").is_file());
    }
}
