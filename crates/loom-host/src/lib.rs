mod driver;
mod link;
mod process;

use std::sync::Arc;

use loom_config::{Dirs, HostConfig, HostDriver};

pub use driver::{
    Agent, CheckItem, CheckStatus, Driver, DriverReport, Progress, cpus, memory_mib, valid_instance,
};
pub use link::{
    AgentSource, CLIENT_NAME, Host, IDENTITY_FILE, JoinOptions, JoinedOracle, LocalAgent,
    ORACLE_FILE, identity, join,
};
pub use process::ProcessDriver;

pub fn driver(config: &HostConfig, dirs: &Dirs) -> anyhow::Result<Arc<dyn Driver>> {
    match config.driver {
        HostDriver::Process => Ok(Arc::new(ProcessDriver::new(config.process_root(dirs)))),
    }
}

pub fn agents(config: &HostConfig) -> Arc<dyn AgentSource> {
    Arc::new(LocalAgent::new(config.agent.path.clone()))
}

pub fn host(config: HostConfig, dirs: &Dirs) -> anyhow::Result<Host> {
    let data_dir = config.data_dir(dirs);
    let oracle = JoinedOracle::load(&data_dir)?;
    let identity = Arc::new(identity(&data_dir)?);
    Ok(Host {
        driver: driver(&config, dirs)?,
        agents: agents(&config),
        config,
        oracle,
        identity,
    })
}
