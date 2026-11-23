use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use loom_client::oracle_proto::{Bootstrap, CreateInstance, Environment, InstanceReport};
use loom_incus::{Exec, FileOwner, Incus, IncusError, Project, StateAction};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::driver::{
    Agent, CheckItem, CheckStatus, Driver, DriverReport, Progress, valid_instance,
};

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
const DEV: u32 = 1000;
const CREATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub struct IncusDriver {
    incus: Incus,
    pool: String,
    network: String,
    build: tokio::sync::Mutex<()>,
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

impl IncusDriver {
    pub fn new(incus: Incus, pool: String, network: String) -> Self {
        Self {
            incus,
            pool,
            network,
            build: tokio::sync::Mutex::new(()),
        }
    }

    pub fn incus(&self) -> &Incus {
        &self.incus
    }

    fn devices(&self) -> Value {
        json!({
            "root": {"type": "disk", "path": "/", "pool": self.pool},
            "eth0": {"type": "nic", "name": "eth0", "network": self.network},
        })
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
            "devices": self.devices(),
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

    pub async fn setup_project(&self) -> anyhow::Result<bool> {
        let project = self
            .incus
            .project()
            .context("set incus.project in host.yaml")?
            .to_owned();
        match self.incus.project_info(&project).await {
            Ok(_) => return Ok(false),
            Err(err) if err.is_not_found() => {}
            Err(err) => return Err(incus_error(err)),
        }
        let config = BTreeMap::from([
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
        self.incus
            .create_project(&Project {
                name: project,
                description: "Loom codespaces".into(),
                config,
            })
            .await
            .map_err(incus_error)?;
        Ok(true)
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
            "devices": self.devices(),
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
        let alias = self
            .ensure_image(&request.environment, oracle, progress)
            .await?;
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
        let body = json!({
            "name": request.instance,
            "type": "container",
            "source": {"type": "image", "alias": alias},
            "config": config,
            "devices": self.devices(),
            "profiles": [],
        });
        self.incus
            .create_instance(&body, CREATE_TIMEOUT)
            .await
            .map_err(incus_error)
            .context("cannot create the instance")?;
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

    async fn check(&self, deep: bool) -> Vec<CheckItem> {
        let mut items = Vec::new();
        let target = self.incus.target();
        match self.incus.server().await {
            Ok(server) if server.auth == "trusted" => items.push(CheckItem::new(
                "incus",
                CheckStatus::Ok,
                format!(
                    "{target}: Incus {} on {} {}",
                    server.environment.server_version,
                    server.environment.kernel_architecture,
                    server.environment.kernel_version
                ),
            )),
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
        }
        let project = self.incus.project().unwrap_or("default").to_owned();
        match self.incus.project_info(&project).await {
            Ok(info) => {
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
            }
            Err(err) => items.push(CheckItem::new(
                "pool",
                CheckStatus::Failed,
                format!("{}: {}", self.pool, err.message()),
            )),
        }
        match self.incus.network(&self.network).await {
            Ok(network) => items.push(CheckItem::new(
                "network",
                CheckStatus::Ok,
                format!("{} ({})", network.name, network.kind),
            )),
            Err(err) => items.push(CheckItem::new(
                "network",
                CheckStatus::Failed,
                format!("{}: {}", self.network, err.message()),
            )),
        }
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
