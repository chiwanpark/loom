mod agents;
mod driver;
mod incus;
mod link;
mod process;
mod providers;
mod volumes;

use std::sync::Arc;

use anyhow::Context;
use loom_config::{Dirs, HostConfig, HostDriver};

pub use agents::{AgentCache, AgentSource};
pub use driver::{
    Agent, CheckItem, CheckStatus, Driver, DriverReport, Input, Output, Progress, cpus, memory_mib,
    valid_instance,
};
pub use incus::{
    AGENT_UNIT, BASE_SCRIPT, DEFAULT_BASE_IMAGE, IMAGE_PREFIX, IncusDriver, KEY_CODESPACE,
    KEY_ORACLE, SetupResult, environment_file, environment_hash, image_source, stdio_exec,
};
pub use link::{
    CLIENT_NAME, Host, IDENTITY_FILE, JoinOptions, JoinedOracle, ORACLE_FILE, identity, join,
};
pub use process::ProcessDriver;

pub fn connect_incus(config: &HostConfig, dirs: &Dirs) -> anyhow::Result<loom_incus::Incus> {
    let project = Some(config.incus.project.clone());
    if let Some(url) = &config.incus.url {
        let certificates = config.incus_certificates(dirs);
        let pinned =
            config.incus.server_certificate.is_some() || certificates.server_certificate.exists();
        let incus = loom_incus::load_https(
            url,
            &certificates.client_certificate,
            &certificates.client_key,
            pinned.then_some(certificates.server_certificate.as_path()),
        )
        .with_context(|| {
            format!(
                "cannot load the Incus certificate {}; run `loom-host trust <token>` first",
                certificates.client_certificate.display()
            )
        })?;
        return Ok(incus.with_project(project));
    }
    let socket = config
        .incus
        .socket
        .clone()
        .or_else(loom_incus::find_socket)
        .context("cannot find the Incus socket; set incus.socket or incus.url in host.yaml")?;
    Ok(loom_incus::Incus::unix(socket).with_project(project))
}

pub fn incus_driver(config: &HostConfig, dirs: &Dirs) -> anyhow::Result<IncusDriver> {
    Ok(IncusDriver::new(
        connect_incus(config, dirs)?,
        config.incus.pool.clone(),
        config.incus.network.clone(),
        config.rules.clone(),
    ))
}

pub fn driver(config: &HostConfig, dirs: &Dirs) -> anyhow::Result<Arc<dyn Driver>> {
    match config.driver {
        HostDriver::Process => Ok(Arc::new(ProcessDriver::new(
            config.process_root(dirs),
            config.rules.clone(),
        ))),
        HostDriver::Incus => Ok(Arc::new(incus_driver(config, dirs)?)),
    }
}

pub fn agents(config: &HostConfig, dirs: &Dirs) -> Arc<AgentCache> {
    Arc::new(AgentCache::new(
        config.agent.path.clone(),
        config.agent_cache(dirs),
    ))
}

pub fn host(config: HostConfig, dirs: &Dirs) -> anyhow::Result<Host> {
    let data_dir = config.data_dir(dirs);
    let oracle = JoinedOracle::load(&data_dir)?;
    let identity = Arc::new(identity(&data_dir)?);
    Ok(Host {
        driver: driver(&config, dirs)?,
        agents: agents(&config, dirs),
        config,
        oracle,
        identity,
        http: loom_github::Http::new()?,
    })
}

pub async fn stdio(
    driver: &dyn Driver,
    oracle: &str,
    codespace: &str,
    input: Input,
    output: Output,
    errors: Output,
) -> anyhow::Result<i32> {
    let instance = driver
        .list(oracle)
        .await?
        .into_iter()
        .find(|instance| instance.codespace == codespace)
        .with_context(|| format!("this host has no codespace {codespace}"))?;
    if !instance.running {
        anyhow::bail!("the codespace {codespace} is stopped; resume it first");
    }
    driver
        .stdio(oracle, &instance.instance, input, output, errors)
        .await
}
