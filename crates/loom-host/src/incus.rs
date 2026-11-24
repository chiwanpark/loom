use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use loom_client::oracle_proto::{
    Bootstrap, CreateInstance, Environment, InstanceReport, InstanceVolume, VolumeKind, VolumeUsage,
};
use loom_config::HostRules;
use loom_incus::{Exec, FileOwner, Incus, IncusError, Instance, Project, StateAction, StoragePool};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::driver::{
    Agent, CheckItem, CheckStatus, Driver, DriverReport, Input, Output, Progress, valid_instance,
};
use crate::volumes::{DEV_GID, DEV_UID, idmap, incus_volume, prepare_mounts};

pub const KEY_ORACLE: &str = "user.loom.oracle";
pub const KEY_CODESPACE: &str = "user.loom.codespace";
pub const KEY_BUILD: &str = "user.loom.build";
pub const PROPERTY_ORACLE: &str = "loom.oracle";
pub const PROPERTY_ENVIRONMENT: &str = "loom.environment";
pub const IMAGE_PREFIX: &str = "loom-image-";
pub const BUILD_PREFIX: &str = "loom-build-";
pub const DEFAULT_BASE_IMAGE: &str = "images:debian/13";
pub const BASE_SCRIPT: &str = include_str!("incus/base.sh");
pub const AGENT_UNIT: &str = include_str!("incus/loom-agent.service");
const AGENT_PATH: &str = "/usr/local/bin/loom-agent";
const UNIT_PATH: &str = "/etc/systemd/system/loom-agent.service";
const UNIT_LINK: &str = "/etc/systemd/system/multi-user.target.wants/loom-agent.service";
const BOOTSTRAP_PATH: &str = "/home/dev/.config/loom/bootstrap.json";
const WORKSPACES: &str = "/workspaces";
const DEV_HOME: &str = "/home/dev";
const DEV: u32 = 1000;
const CREATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct IncusDriver {
    incus: Incus,
    pool: String,
    network: String,
    rules: HostRules,
    build: tokio::sync::Mutex<()>,
    volumes: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupResult {
    Created,
    Updated(Vec<String>),
    Unchanged,
}

fn subordinate_entry(file: &str, id: u32) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else {
        return false;
    };
    text.lines().any(|line| {
        let parts: Vec<&str> = line.trim().split(':').collect();
        let [user, start, count] = parts[..] else {
            return false;
        };
        let (Ok(start), Ok(count)) = (start.parse::<u64>(), count.parse::<u64>()) else {
            return false;
        };
        user == "root" && start <= u64::from(id) && u64::from(id) < start + count
    })
}

fn project_quotas(source: &Path, mountinfo: &str) -> Option<bool> {
    let (_, options) = mountinfo
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            let mount_point = mount
                .split_whitespace()
                .nth(4)?
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\");
            let mount_point = Path::new(&mount_point);
            let depth = mount_point.components().count();
            let options = filesystem.split_whitespace().nth(2)?;
            source.starts_with(mount_point).then_some((depth, options))
        })
        .max_by_key(|(depth, _)| *depth)?;
    Some(
        options
            .split(',')
            .any(|opt| matches!(opt, "prjquota" | "pquota")),
    )
}

fn local_project_quotas(pool: &StoragePool) -> Option<bool> {
    let path = std::fs::canonicalize(pool.config.as_ref()?.get("source")?).ok()?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    project_quotas(&path, &mountinfo)
}

pub fn image_source(base: &str) -> anyhow::Result<Value> {
    let remote = |server: &str, alias: &str| {
        json!({
            "type": "image",
            "mode": "pull",
            "server": server,
            "protocol": "simplestreams",
            "alias": alias,
        })
    };
    match base.split_once(':') {
        Some(("images", alias)) if !alias.is_empty() => {
            Ok(remote("https://images.linuxcontainers.org", alias))
        }
        Some(("ubuntu", alias)) if !alias.is_empty() => {
            Ok(remote("https://cloud-images.ubuntu.com/releases", alias))
        }
        Some(("local", alias)) if !alias.is_empty() => Ok(json!({"type": "image", "alias": alias})),
        None if !base.is_empty() => Ok(json!({"type": "image", "alias": base})),
        _ => bail!(
            "the base image {base:?} must be images:<alias>, ubuntu:<alias>, or the alias of a local image"
        ),
    }
}

pub fn environment_hash(environment: &Environment) -> String {
    let input = json!({
        "format": 2,
        "base_script": BASE_SCRIPT,
        "environment": environment,
    });
    let digest = Sha256::digest(input.to_string().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

pub fn environment_file(environment: &Environment) -> String {
    environment
        .variables
        .iter()
        .map(|(name, value)| {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{name}=\"{escaped}\"\n")
        })
        .collect()
}

fn script_env(environment: &Environment) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([
        ("PATH".to_owned(), PATH.to_owned()),
        ("HOME".to_owned(), "/root".to_owned()),
        ("USER".to_owned(), "root".to_owned()),
        ("LANG".to_owned(), "C.UTF-8".to_owned()),
        ("DEBIAN_FRONTEND".to_owned(), "noninteractive".to_owned()),
    ]);
    env.extend(environment.variables.clone());
    env
}

fn incus_error(err: IncusError) -> anyhow::Error {
    anyhow::anyhow!("{}", err.message())
}

pub fn stdio_exec() -> Exec {
    Exec {
        command: vec![AGENT_PATH.into(), "stdio".into(), "--managed".into()],
        environment: BTreeMap::from([
            ("HOME".to_owned(), DEV_HOME.to_owned()),
            ("USER".to_owned(), "dev".to_owned()),
            ("PATH".to_owned(), PATH.to_owned()),
            ("LANG".to_owned(), "C.UTF-8".to_owned()),
        ]),
        user: DEV,
        group: DEV,
        cwd: Some(DEV_HOME.into()),
    }
}

impl IncusDriver {
    pub fn new(incus: Incus, pool: String, network: String, rules: HostRules) -> Self {
        Self {
            incus,
            pool,
            network,
            rules,
            build: tokio::sync::Mutex::new(()),
            volumes: tokio::sync::Mutex::new(()),
        }
    }

    pub fn incus(&self) -> &Incus {
        &self.incus
    }

    fn base_devices(&self) -> Value {
        json!({
            "root": {"type": "disk", "path": "/", "pool": self.pool},
            "eth0": self.nic(),
        })
    }

    fn nic(&self) -> Value {
        let rules = &self.rules.network;
        let mut nic = json!({"type": "nic", "name": "eth0", "network": self.network});
        if rules.isolate {
            for key in [
                "security.port_isolation",
                "security.mac_filtering",
                "security.ipv4_filtering",
                "security.ipv6_filtering",
            ] {
                nic[key] = Value::String("true".into());
            }
        }
        if !rules.acls.is_empty() {
            nic["security.acls"] = Value::String(rules.acls.join(","));
            nic["security.acls.default.egress.action"] =
                Value::String(rules.default_egress.as_str().into());
            nic["security.acls.default.ingress.action"] =
                Value::String(rules.default_ingress.as_str().into());
        }
        nic
    }

    fn devices(&self, volumes: &[InstanceVolume]) -> anyhow::Result<Value> {
        let mut devices = self.base_devices();
        for (index, volume) in volumes.iter().enumerate() {
            let device = match (volume.kind, &volume.source) {
                (VolumeKind::Incus, None) => json!({
                    "type": "disk",
                    "pool": self.pool,
                    "source": incus_volume(&volume.name)?,
                    "path": volume.path,
                }),
                (VolumeKind::Directory, Some(source)) => json!({
                    "type": "disk",
                    "source": source,
                    "path": volume.path,
                }),
                _ => bail!("the volume {} has an unknown kind", volume.name),
            };
            let mut device = device;
            if volume.read_only {
                device["readonly"] = Value::String("true".into());
            }
            devices[format!("volume{index}")] = device;
        }
        Ok(devices)
    }

    async fn ensure_volumes(
        &self,
        oracle: &str,
        volumes: &[InstanceVolume],
        progress: &Progress,
    ) -> anyhow::Result<()> {
        for volume in volumes.iter().filter(|v| v.kind == VolumeKind::Incus) {
            let name = incus_volume(&volume.name)?;
            match self
                .incus
                .storage_volume(&self.pool, &name)
                .await
                .map_err(incus_error)?
            {
                Some(found) => {
                    let owner = found.config.get(KEY_ORACLE).map(String::as_str);
                    if owner != Some(oracle) {
                        bail!(
                            "the storage volume {name} exists already, but it does not belong to this oracle"
                        );
                    }
                }
                None => {
                    progress.step("volume");
                    let mut config = BTreeMap::from([
                        (KEY_ORACLE.to_owned(), oracle.to_owned()),
                        ("initial.uid".to_owned(), DEV_UID.to_string()),
                        ("initial.gid".to_owned(), DEV_GID.to_string()),
                        ("initial.mode".to_owned(), "0755".to_owned()),
                    ]);
                    if let Some(size) = volume.size_mib {
                        config.insert("size".to_owned(), format!("{size}MiB"));
                    }
                    self.incus
                        .create_storage_volume(&self.pool, &name, &config)
                        .await
                        .map_err(incus_error)
                        .with_context(|| format!("cannot create the storage volume {name}"))?;
                }
            }
        }
        Ok(())
    }

    async fn remove(&self, name: &str) -> anyhow::Result<()> {
        let Some(instance) = self.incus.instance(name).await.map_err(incus_error)? else {
            return Ok(());
        };
        if instance.running() {
            self.incus
                .set_state(name, StateAction::Stop, true)
                .await
                .map_err(incus_error)?;
        }
        self.incus.delete_instance(name).await.map_err(incus_error)
    }

    async fn owned(&self, oracle: &str, instance: &str) -> anyhow::Result<Instance> {
        if !valid_instance(instance) {
            bail!("{instance:?} is not an instance name of Loom");
        }
        let found = self
            .incus
            .instance(instance)
            .await
            .map_err(incus_error)?
            .with_context(|| format!("the instance {instance} is not there"))?;
        if found.config.get(KEY_ORACLE).map(String::as_str) != Some(oracle) {
            bail!("the instance {instance} does not belong to this oracle");
        }
        Ok(found)
    }

    async fn wait_network(&self, name: &str) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + NETWORK_TIMEOUT;
        loop {
            let state = self.incus.instance_state(name).await.map_err(incus_error)?;
            if state.global_ipv4().is_some() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("the instance {name} got no IPv4 address");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn run_script(
        &self,
        name: &str,
        path: &str,
        script: &str,
        environment: &Environment,
    ) -> anyhow::Result<()> {
        self.incus
            .push_file(
                name,
                path,
                script.as_bytes().to_vec(),
                FileOwner::root(0o700),
            )
            .await
            .map_err(incus_error)?;
        let exec = Exec {
            command: vec!["/bin/bash".into(), path.into()],
            environment: script_env(environment),
            cwd: Some("/root".into()),
            ..Exec::default()
        };
        let output = self
            .incus
            .exec(name, &exec, SCRIPT_TIMEOUT)
            .await
            .map_err(incus_error)?;
        if !output.success() {
            bail!(
                "{path} ended with exit code {}:\n{}",
                output.code,
                output.stderr_tail(20)
            );
        }
        Ok(())
    }

    async fn prepare_build(
        &self,
        name: &str,
        environment: &Environment,
        progress: &Progress,
    ) -> anyhow::Result<()> {
        let incus = &self.incus;
        self.wait_network(name).await?;
        let files: Vec<(String, String)> = std::iter::once((
            "/etc/loom/environment".to_owned(),
            environment_file(environment),
        ))
        .chain(
            environment
                .ca_certificates
                .iter()
                .enumerate()
                .map(|(i, pem)| {
                    (
                        format!("/usr/local/share/ca-certificates/loom-{i}.crt"),
                        pem.clone(),
                    )
                }),
        )
        .chain((!environment.docker.registry_mirrors.is_empty()).then(|| {
            (
                "/etc/docker/daemon.json".to_owned(),
                json!({"registry-mirrors": environment.docker.registry_mirrors}).to_string(),
            )
        }))
        .collect();
        for dir in [
            "/etc/loom",
            "/etc/docker",
            "/usr/local/share/ca-certificates",
        ] {
            incus
                .push_directory(name, dir, FileOwner::root(0o755))
                .await
                .map_err(incus_error)?;
        }
        for (path, content) in files {
            incus
                .push_file(name, &path, content.into_bytes(), FileOwner::root(0o644))
                .await
                .map_err(incus_error)?;
        }
        progress.step("image: base script");
        self.run_script(name, "/root/loom-base.sh", BASE_SCRIPT, environment)
            .await?;
        if let Some(script) = &environment.setup_script {
            progress.step("image: setup script");
            self.run_script(name, "/root/loom-setup.sh", script, environment)
                .await?;
        }
        let cleanup = "apt-get clean; rm -rf /var/lib/apt/lists/* /root/loom-*.sh /tmp/*; true";
        let exec = Exec {
            command: vec!["/bin/sh".into(), "-c".into(), cleanup.into()],
            environment: script_env(environment),
            ..Exec::default()
        };
        incus
            .exec(name, &exec, SCRIPT_TIMEOUT)
            .await
            .map_err(incus_error)?;
        Ok(())
    }

    async fn publish(
        &self,
        name: &str,
        alias: &str,
        properties: &BTreeMap<String, String>,
    ) -> anyhow::Result<String> {
        match self
            .incus
            .publish(name, alias, properties, Some("zstd"), CREATE_TIMEOUT)
            .await
        {
            Ok(fingerprint) => Ok(fingerprint),
            Err(err) if err.message().contains("zstd") => {
                tracing::warn!(error = %err.message(), "the Incus host cannot use zstd; publishing without compression");
                self.incus
                    .publish(name, alias, properties, Some("none"), CREATE_TIMEOUT)
                    .await
                    .map_err(incus_error)
            }
            Err(err) => Err(incus_error(err)),
        }
    }

    pub async fn ensure_image(
        &self,
        environment: &Environment,
        oracle: &str,
        progress: &Progress,
    ) -> anyhow::Result<String> {
        let hash = environment_hash(environment);
        let alias = format!("{IMAGE_PREFIX}{hash}");
        let _build = self.build.lock().await;
        if self
            .incus
            .image_alias(&alias)
            .await
            .map_err(incus_error)?
            .is_some()
        {
            return Ok(alias);
        }
        progress.step("image");
        let base = environment
            .base_image
            .as_deref()
            .unwrap_or(DEFAULT_BASE_IMAGE);
        tracing::info!(%alias, %base, "building the codespace image");
        let name = format!("{BUILD_PREFIX}{}", &hash[..12]);
        self.remove(&name).await?;
        let body = json!({
            "name": name,
            "type": "container",
            "source": image_source(base)?,
            "config": {
                "security.nesting": "true",
                KEY_ORACLE: oracle,
                KEY_BUILD: hash,
            },
            "devices": self.base_devices(),
            "profiles": [],
        });
        self.incus
            .create_instance(&body, CREATE_TIMEOUT)
            .await
            .map_err(incus_error)
            .with_context(|| format!("cannot create an instance from {base}"))?;
        let built = async {
            self.incus
                .set_state(&name, StateAction::Start, false)
                .await
                .map_err(incus_error)?;
            self.prepare_build(&name, environment, progress).await?;
            self.incus
                .set_state(&name, StateAction::Stop, false)
                .await
                .map_err(incus_error)?;
            progress.step("image: publish");
            let properties = BTreeMap::from([
                (PROPERTY_ORACLE.to_owned(), oracle.to_owned()),
                (PROPERTY_ENVIRONMENT.to_owned(), hash.clone()),
                (
                    "description".to_owned(),
                    format!("Loom codespace image {hash} from {base}"),
                ),
            ]);
            self.publish(&name, &alias, &properties).await
        }
        .await;
        if let Err(err) = self.remove(&name).await {
            tracing::warn!(error = %format!("{err:#}"), instance = %name, "cannot delete the build instance");
        }
        let fingerprint = built.context("cannot build the codespace image")?;
        tracing::info!(%alias, %fingerprint, "built the codespace image");
        match self.incus.images().await {
            Ok(images) => {
                for image in images {
                    let old = image.properties.get(PROPERTY_ORACLE).map(String::as_str)
                        == Some(oracle)
                        && image.fingerprint != fingerprint
                        && image
                            .aliases
                            .iter()
                            .all(|a| a.name.starts_with(IMAGE_PREFIX));
                    if old && let Err(err) = self.incus.delete_image(&image.fingerprint).await {
                        tracing::warn!(error = %err, "cannot delete an old codespace image");
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "cannot list the images"),
        }
        Ok(alias)
    }

    fn volume_keys(&self) -> BTreeMap<String, String> {
        let mut keys = BTreeMap::new();
        if !self.rules.directories.is_empty() {
            let paths: Vec<String> = self
                .rules
                .directories
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            keys.insert("restricted.devices.disk".to_owned(), "allow".to_owned());
            keys.insert("restricted.devices.disk.paths".to_owned(), paths.join(","));
        }
        if let Some(user) = self.rules.host_user {
            keys.insert("restricted.idmap.uid".to_owned(), user.uid.to_string());
            keys.insert("restricted.idmap.gid".to_owned(), user.gid.to_string());
        }
        keys
    }

    pub async fn setup_project(&self) -> anyhow::Result<SetupResult> {
        let project = self
            .incus
            .project()
            .context("set incus.project in host.yaml")?
            .to_owned();
        let keys = self.volume_keys();
        match self.incus.project_info(&project).await {
            Ok(info) => {
                let missing: BTreeMap<String, String> = keys
                    .into_iter()
                    .filter(|(key, value)| info.config.get(key) != Some(value))
                    .collect();
                if missing.is_empty() {
                    return Ok(SetupResult::Unchanged);
                }
                self.incus
                    .update_project(&project, &missing)
                    .await
                    .map_err(incus_error)?;
                return Ok(SetupResult::Updated(missing.into_keys().collect()));
            }
            Err(err) if err.is_not_found() => {}
            Err(err) => return Err(incus_error(err)),
        }
        let mut config = BTreeMap::from([
            ("features.images".to_owned(), "true".to_owned()),
            ("features.profiles".to_owned(), "true".to_owned()),
            ("features.storage.volumes".to_owned(), "true".to_owned()),
            ("restricted".to_owned(), "true".to_owned()),
            (
                "restricted.containers.nesting".to_owned(),
                "allow".to_owned(),
            ),
            (
                "restricted.networks.access".to_owned(),
                self.network.clone(),
            ),
        ]);
        config.extend(keys);
        self.incus
            .create_project(&Project {
                name: project,
                description: "Loom codespaces".into(),
                config,
            })
            .await
            .map_err(incus_error)?;
        Ok(SetupResult::Created)
    }

    fn check_volumes(
        &self,
        project: Option<&BTreeMap<String, String>>,
        pool: Option<&StoragePool>,
        initial_owner: bool,
    ) -> CheckItem {
        let mut problems = Vec::new();
        let mut notes = Vec::new();
        if let Some(config) = project
            && config.get("restricted").map(String::as_str) == Some("true")
        {
            let missing: Vec<String> = self
                .volume_keys()
                .into_iter()
                .filter(|(key, value)| config.get(key) != Some(value))
                .map(|(key, _)| key)
                .collect();
            if !missing.is_empty() {
                problems.push(format!(
                    "the project lacks {}; run `loom-host setup`",
                    missing.join(", ")
                ));
            }
            if config.get("features.storage.volumes").map(String::as_str) != Some("true") {
                problems.push("set features.storage.volumes=true for this project".to_owned());
            }
        }
        if let Some(user) = self.rules.host_user {
            if self.incus.is_local() {
                for (file, id) in [("/etc/subuid", user.uid), ("/etc/subgid", user.gid)] {
                    if !subordinate_entry(file, id) {
                        problems.push(format!("add root:{id}:1 to {file}, then restart Incus"));
                    }
                }
            } else {
                notes.push("cannot check /etc/subuid and /etc/subgid of a remote Incus".to_owned());
            }
        }
        for directory in &self.rules.directories {
            if !self.incus.is_local() {
                problems.push("host directories need the local Incus socket".to_owned());
                break;
            }
            if let Err(err) =
                crate::volumes::host_directory(&self.rules, &directory.to_string_lossy())
            {
                problems.push(err);
            }
        }
        if !self.rules.directories.is_empty() && self.rules.host_user.is_none() {
            problems.push("set rules.host_user to mount host directories".to_owned());
        }
        if !initial_owner {
            notes.push("Incus does not advertise storage_initial_owner; update Incus to use custom volumes".to_owned());
        }
        match pool.map(|pool| pool.driver.as_str()) {
            Some("dir") => {
                let quotas = pool
                    .filter(|_| self.incus.is_local())
                    .and_then(local_project_quotas);
                notes.push(match quotas {
                    Some(true) => "project quotas detected on the dir pool filesystem",
                    Some(false) => "no project quotas detected on the dir pool filesystem; size limits may not apply",
                    None => "cannot check project quotas of the dir pool; size limits require project quotas",
                }.to_owned());
            }
            Some("zfs" | "btrfs" | "lvm") => {
                notes.push("the pool supports volume size limits".to_owned())
            }
            None => {}
            Some(other) => notes.push(format!("the {other} pool may not limit sizes")),
        }
        if problems.is_empty() {
            let mut detail = if self.rules.directories.is_empty() {
                "Incus volumes".to_owned()
            } else {
                format!(
                    "Incus volumes and host directories ({} roots)",
                    self.rules.directories.len()
                )
            };
            for note in notes {
                detail.push_str(&format!("; {note}"));
            }
            CheckItem::new("volumes", CheckStatus::Ok, detail)
        } else {
            CheckItem::new("volumes", CheckStatus::Failed, problems.join("; "))
        }
    }

    async fn check_network(&self) -> CheckItem {
        let rules = &self.rules.network;
        let network = match self.incus.network(&self.network).await {
            Ok(network) => network,
            Err(err) => {
                return CheckItem::new(
                    "network",
                    CheckStatus::Failed,
                    format!("{}: {}", self.network, err.message()),
                );
            }
        };
        let mut detail = format!("{} ({})", network.name, network.kind);
        if rules.isolate && network.kind != "bridge" {
            return CheckItem::new(
                "network",
                CheckStatus::Failed,
                format!(
                    "{detail}: the isolation of codespaces needs a bridge network; set rules.network.isolate: false"
                ),
            );
        }
        detail.push_str(if rules.isolate {
            "; codespaces cannot reach each other or use other addresses"
        } else {
            "; codespaces are not isolated from each other"
        });
        let mut status = CheckStatus::Ok;
        let mut problems = Vec::new();
        for acl in &rules.acls {
            match self.incus.network_acl(acl).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    status = CheckStatus::Failed;
                    problems.push(format!("the network ACL {acl} does not exist"));
                }
                Err(err) => {
                    if status == CheckStatus::Ok {
                        status = CheckStatus::Warning;
                    }
                    problems.push(format!(
                        "cannot read the network ACL {acl}: {}",
                        err.message()
                    ));
                }
            }
        }
        if !rules.acls.is_empty() {
            detail.push_str(&format!(
                "; ACLs {} (default egress {}, ingress {})",
                rules.acls.join(", "),
                rules.default_egress.as_str(),
                rules.default_ingress.as_str()
            ));
        }
        for problem in problems {
            detail.push_str(&format!("; {problem}"));
        }
        CheckItem::new("network", status, detail)
    }

    async fn check_docker(&self, items: &mut Vec<CheckItem>) {
        let image = match self.incus.images().await {
            Ok(images) => images
                .into_iter()
                .filter_map(|image| {
                    let alias = image
                        .aliases
                        .iter()
                        .find(|a| a.name.starts_with(IMAGE_PREFIX))?;
                    Some((image.created_at.clone(), alias.name.clone()))
                })
                .max(),
            Err(err) => {
                items.push(CheckItem::new("docker", CheckStatus::Failed, err.message()));
                return;
            }
        };
        let Some((_, alias)) = image else {
            items.push(CheckItem::new(
                "docker",
                CheckStatus::Warning,
                "there is no codespace image yet; the first codespace builds it",
            ));
            return;
        };
        let mut bytes = [0u8; 4];
        rand::fill(&mut bytes);
        let name = format!(
            "loom-check-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let body = json!({
            "name": name,
            "type": "container",
            "source": {"type": "image", "alias": alias},
            "config": {"security.nesting": "true"},
            "devices": self.base_devices(),
            "profiles": [],
        });
        let result = async {
            self.incus
                .create_instance(&body, CREATE_TIMEOUT)
                .await
                .map_err(incus_error)?;
            self.incus
                .set_state(&name, StateAction::Start, false)
                .await
                .map_err(incus_error)?;
            self.wait_network(&name).await?;
            let script = "for i in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done; \
                docker info --format '{{.ServerVersion}} {{.Driver}}' && docker run --rm hello-world >/dev/null && echo hello-world";
            let exec = Exec {
                command: vec!["/bin/sh".into(), "-c".into(), script.into()],
                environment: script_env(&Environment::default()),
                ..Exec::default()
            };
            let output = self
                .incus
                .exec(&name, &exec, Duration::from_secs(300))
                .await
                .map_err(incus_error)?;
            if !output.success() {
                bail!("{}", output.stderr_tail(5));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().replace('\n', ", "))
        }
        .await;
        if let Err(err) = self.remove(&name).await {
            tracing::warn!(error = %format!("{err:#}"), "cannot delete the check instance");
        }
        match result {
            Ok(detail) => items.push(CheckItem::new(
                "docker",
                CheckStatus::Ok,
                format!("{detail} in {alias}"),
            )),
            Err(err) => items.push(CheckItem::new(
                "docker",
                CheckStatus::Failed,
                format!("{err:#}"),
            )),
        }
    }
}

#[async_trait]
impl Driver for IncusDriver {
    async fn report(&self) -> anyhow::Result<DriverReport> {
        let server = self.incus.server().await.map_err(incus_error)?;
        let resources = self.incus.resources().await.map_err(incus_error)?;
        let storage = self
            .incus
            .storage_pool(&self.pool)
            .await
            .map(|pool| pool.driver)
            .ok();
        Ok(DriverReport {
            driver: "incus".into(),
            cpus: u32::try_from(resources.cpu.total).unwrap_or(u32::MAX),
            memory_mib: resources.memory.total / (1 << 20),
            architecture: Some(server.environment.kernel_architecture)
                .filter(|arch| !arch.is_empty()),
            storage_driver: storage,
            incus_version: Some(server.environment.server_version),
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
        if self
            .incus
            .instance(&request.instance)
            .await
            .map_err(incus_error)?
            .is_some()
        {
            bail!("the instance {} exists already", request.instance);
        }
        let volumes = prepare_mounts(&self.rules, self.local(), &request.volumes)?;
        let alias = self
            .ensure_image(&request.environment, oracle, progress)
            .await?;
        let devices = self.devices(&volumes)?;
        let volume_guard = self.volumes.lock().await;
        self.ensure_volumes(oracle, &volumes, progress).await?;
        progress.step("instance");
        let mut config = json!({
            "limits.cpu": request.cpus.to_string(),
            "limits.memory": format!("{}MiB", request.memory_mib),
            "security.nesting": "true",
            KEY_ORACLE: oracle,
            KEY_CODESPACE: request.codespace,
        });
        if let Some(version) = &agent.version {
            config["user.loom.agent_version"] = Value::String(version.clone());
        }
        if let Some(user) = self.rules.host_user {
            config["raw.idmap"] = Value::String(idmap(user));
        }
        let body = json!({
            "name": request.instance,
            "type": "container",
            "source": {"type": "image", "alias": alias},
            "config": config,
            "devices": devices,
            "profiles": [],
        });
        self.incus
            .create_instance(&body, CREATE_TIMEOUT)
            .await
            .map_err(incus_error)
            .context("cannot create the instance")?;
        drop(volume_guard);
        progress.step("agent");
        let binary = tokio::fs::read(&agent.path)
            .await
            .with_context(|| format!("cannot read {}", agent.path.display()))?;
        self.incus
            .push_file(
                &request.instance,
                AGENT_PATH,
                binary,
                FileOwner::root(0o755),
            )
            .await
            .map_err(incus_error)?;
        self.incus
            .push_file(
                &request.instance,
                UNIT_PATH,
                AGENT_UNIT.as_bytes().to_vec(),
                FileOwner::root(0o644),
            )
            .await
            .map_err(incus_error)?;
        self.incus
            .push_symlink(&request.instance, UNIT_LINK, UNIT_PATH)
            .await
            .map_err(incus_error)?;
        let bootstrap = Bootstrap {
            workspaces_root: Some(WORKSPACES.into()),
            ..request.bootstrap.clone()
        };
        self.incus
            .push_file(
                &request.instance,
                BOOTSTRAP_PATH,
                serde_json::to_vec_pretty(&bootstrap)?,
                FileOwner {
                    uid: DEV,
                    gid: DEV,
                    mode: 0o600,
                },
            )
            .await
            .map_err(incus_error)?;
        progress.step("start");
        self.incus
            .set_state(&request.instance, StateAction::Start, false)
            .await
            .map_err(incus_error)?;
        Ok(())
    }

    async fn delete(&self, oracle: &str, instance: &str) -> anyhow::Result<()> {
        if !valid_instance(instance) {
            bail!("{instance:?} is not an instance name of Loom");
        }
        let Some(found) = self.incus.instance(instance).await.map_err(incus_error)? else {
            return Ok(());
        };
        if found.config.get(KEY_ORACLE).map(String::as_str) != Some(oracle) {
            bail!("the instance {instance} does not belong to this oracle");
        }
        self.remove(instance).await
    }

    async fn stop(&self, oracle: &str, instance: &str) -> anyhow::Result<()> {
        if !self.owned(oracle, instance).await?.running() {
            return Ok(());
        }
        if let Err(err) = self
            .incus
            .set_state(instance, StateAction::Stop, false)
            .await
        {
            tracing::warn!(%instance, error = %err, "the instance did not stop in time; forcing it");
            self.incus
                .set_state(instance, StateAction::Stop, true)
                .await
                .map_err(incus_error)?;
        }
        Ok(())
    }

    async fn start(&self, oracle: &str, instance: &str) -> anyhow::Result<()> {
        if self.owned(oracle, instance).await?.running() {
            return Ok(());
        }
        self.incus
            .set_state(instance, StateAction::Start, false)
            .await
            .map_err(incus_error)
    }

    async fn stdio(
        &self,
        oracle: &str,
        instance: &str,
        input: Input,
        output: Output,
        errors: Output,
    ) -> anyhow::Result<i32> {
        if !self.owned(oracle, instance).await?.running() {
            bail!("the instance {instance} is stopped; resume the codespace first");
        }
        self.incus
            .exec_stream(instance, &stdio_exec(), input, output, errors)
            .await
            .map_err(incus_error)
    }

    async fn list(&self, oracle: &str) -> anyhow::Result<Vec<InstanceReport>> {
        let instances = self.incus.instances().await.map_err(incus_error)?;
        Ok(instances
            .into_iter()
            .filter(|instance| instance.config.get(KEY_ORACLE).map(String::as_str) == Some(oracle))
            .filter_map(|instance| {
                let codespace = instance.config.get(KEY_CODESPACE)?.clone();
                Some(InstanceReport {
                    running: instance.running(),
                    codespace,
                    instance: instance.name,
                    oracle: oracle.to_owned(),
                })
            })
            .collect())
    }

    fn local(&self) -> bool {
        self.incus.is_local()
    }

    async fn volume_usage(&self, oracle: &str, volumes: &[String]) -> Vec<VolumeUsage> {
        let mut usages = Vec::with_capacity(volumes.len());
        for volume in volumes {
            let result: anyhow::Result<Option<(u64, Option<u64>)>> = async {
                let name = incus_volume(volume)?;
                let Some(found) = self
                    .incus
                    .storage_volume(&self.pool, &name)
                    .await
                    .map_err(incus_error)?
                else {
                    return Ok(None);
                };
                if found.config.get(KEY_ORACLE).map(String::as_str) != Some(oracle) {
                    bail!("the storage volume {name} does not belong to this oracle");
                }
                let state = self
                    .incus
                    .storage_volume_state(&self.pool, &name)
                    .await
                    .map_err(incus_error)?;
                Ok(state.and_then(|s| s.usage).map(|usage| {
                    let total = u64::try_from(usage.total).ok().filter(|t| *t > 0);
                    (usage.used, total)
                }))
            }
            .await;
            let (used_bytes, total_bytes, error) = match result {
                Ok(Some((used, total))) => (Some(used), total, None),
                Ok(None) => (
                    None,
                    None,
                    Some("the volume is not on this host".to_owned()),
                ),
                Err(err) => (None, None, Some(format!("{err:#}"))),
            };
            usages.push(VolumeUsage {
                name: volume.clone(),
                used_bytes,
                total_bytes,
                error,
            });
        }
        usages
    }

    async fn delete_volume(&self, oracle: &str, volume: &str) -> anyhow::Result<()> {
        let _volume_guard = self.volumes.lock().await;
        let name = incus_volume(volume)?;
        let Some(found) = self
            .incus
            .storage_volume(&self.pool, &name)
            .await
            .map_err(incus_error)?
        else {
            return Ok(());
        };
        if found.config.get(KEY_ORACLE).map(String::as_str) != Some(oracle) {
            bail!("the storage volume {name} does not belong to this oracle");
        }
        if !found.used_by.is_empty() {
            bail!(
                "the storage volume {name} is in use by {}",
                found.used_by.join(", ")
            );
        }
        self.incus
            .delete_storage_volume(&self.pool, &name)
            .await
            .map_err(incus_error)
            .with_context(|| format!("cannot delete the storage volume {name}"))
    }

    async fn check(&self, deep: bool) -> Vec<CheckItem> {
        let mut items = Vec::new();
        let target = self.incus.target();
        let server = match self.incus.server().await {
            Ok(server) if server.auth == "trusted" => {
                items.push(CheckItem::new(
                    "incus",
                    CheckStatus::Ok,
                    format!(
                        "{target}: Incus {} on {} {}",
                        server.environment.server_version,
                        server.environment.kernel_architecture,
                        server.environment.kernel_version
                    ),
                ));
                server
            }
            Ok(_) => {
                items.push(CheckItem::new(
                    "incus",
                    CheckStatus::Failed,
                    format!("{target} does not trust this client; run `loom-host trust <token>`"),
                ));
                return items;
            }
            Err(err) => {
                items.push(CheckItem::new(
                    "incus",
                    CheckStatus::Failed,
                    err.to_string(),
                ));
                return items;
            }
        };
        let project = self.incus.project().unwrap_or("default").to_owned();
        let mut project_config = None;
        match self.incus.project_info(&project).await {
            Ok(info) => {
                project_config = Some(info.config.clone());
                let restricted = info.config.get("restricted").map(String::as_str) == Some("true");
                let nesting = info
                    .config
                    .get("restricted.containers.nesting")
                    .map(String::as_str);
                if restricted && nesting != Some("allow") {
                    items.push(CheckItem::new(
                        "project",
                        CheckStatus::Failed,
                        format!("set restricted.containers.nesting=allow on the project {project}"),
                    ));
                } else {
                    let mut detail = project.clone();
                    if let Some(limit) = info.config.get("limits.instances") {
                        detail.push_str(&format!(", at most {limit} instances"));
                    }
                    items.push(CheckItem::new("project", CheckStatus::Ok, detail));
                }
            }
            Err(err) if err.is_not_found() && self.incus.is_local() => items.push(CheckItem::new(
                "project",
                CheckStatus::Failed,
                format!("there is no project {project}; run `loom-host setup`"),
            )),
            Err(err) => items.push(CheckItem::new(
                "project",
                CheckStatus::Failed,
                format!("cannot read the project {project}: {}", err.message()),
            )),
        }
        let mut storage_pool = None;
        match self.incus.storage_pool(&self.pool).await {
            Ok(pool) => {
                let note = match pool.driver.as_str() {
                    "zfs" | "btrfs" => ", new instances are clones of the image",
                    "dir" => ", new instances are full copies of the image; keep the image small",
                    _ => "",
                };
                items.push(CheckItem::new(
                    "pool",
                    CheckStatus::Ok,
                    format!("{} ({}){note}", pool.name, pool.driver),
                ));
                storage_pool = Some(pool);
            }
            Err(err) => items.push(CheckItem::new(
                "pool",
                CheckStatus::Failed,
                format!("{}: {}", self.pool, err.message()),
            )),
        }
        items.push(
            self.check_volumes(
                project_config.as_ref(),
                storage_pool.as_ref(),
                server
                    .api_extensions
                    .iter()
                    .any(|ext| ext == "storage_initial_owner"),
            ),
        );
        items.push(self.check_network().await);
        match self.incus.instances().await {
            Ok(instances) => {
                let codespaces = instances
                    .iter()
                    .filter(|i| i.config.contains_key(KEY_CODESPACE))
                    .count();
                items.push(CheckItem::new(
                    "instances",
                    CheckStatus::Ok,
                    format!(
                        "{} instances, {codespaces} of them codespaces",
                        instances.len()
                    ),
                ));
            }
            Err(err) => items.push(CheckItem::new(
                "instances",
                CheckStatus::Failed,
                err.message(),
            )),
        }
        if deep {
            self.check_docker(&mut items).await;
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_pool_quotas_come_from_the_mount_covering_the_pool_source() {
        let mountinfo = "20 1 0:1 / / rw - ext4 /dev/root rw\n\
                         21 20 0:2 / /srv/dev\\040pool rw - xfs /dev/data rw,prjquota\n\
                         22 21 0:3 / /srv/dev\\040pool/no-quota rw - xfs /dev/dev rw,noquota\n";
        assert_eq!(
            project_quotas(Path::new("/srv/pool"), mountinfo),
            Some(false)
        );
        assert_eq!(
            project_quotas(Path::new("/srv/dev pool/volumes"), mountinfo),
            Some(true)
        );
        assert_eq!(
            project_quotas(Path::new("/srv/dev pool/no-quota/volumes"), mountinfo),
            Some(false)
        );
        assert_eq!(project_quotas(Path::new("/srv/pool"), ""), None);
    }

    #[test]
    fn base_images_come_from_known_remotes() {
        assert_eq!(
            image_source("images:debian/13").unwrap()["server"],
            "https://images.linuxcontainers.org"
        );
        assert_eq!(image_source("ubuntu:24.04").unwrap()["alias"], "24.04");
        assert!(image_source("local:base").unwrap().get("server").is_none());
        assert_eq!(image_source("base").unwrap()["alias"], "base");
        for bad in ["", "images:", "docker:debian"] {
            assert!(image_source(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_image_changes_with_the_environment() {
        let mut environment = Environment::default();
        let first = environment_hash(&environment);
        assert_eq!(first.len(), 16);
        assert_eq!(environment_hash(&environment), first);
        environment
            .variables
            .insert("HTTP_PROXY".into(), "http://proxy:3128".into());
        assert_ne!(environment_hash(&environment), first);
        assert_eq!(
            environment_file(&environment),
            "HTTP_PROXY=\"http://proxy:3128\"\n"
        );
        environment
            .variables
            .insert("Q".into(), "a \"b\" \\c".into());
        assert!(environment_file(&environment).contains("Q=\"a \\\"b\\\" \\\\c\"\n"));
    }
}
