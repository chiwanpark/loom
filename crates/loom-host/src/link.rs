use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use loom_client::oracle_proto::{
    CONNECTION_RELAY, CreateInstance, ErrorCode, HostCall, HostCheck, HostCheckStatus, HostChecks,
    HostReport, Instances, OracleError, VolumeUsages,
};
use loom_client::{
    AccessToken, ClientIdentity, OracleCalls, OracleClient, OracleEndpoint, pair_oracle,
};
use loom_config::HostConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agents::AgentSource;
use crate::driver::{CheckStatus, Driver, Progress};

pub const ORACLE_FILE: &str = "oracle.json";
pub const IDENTITY_FILE: &str = "identity.json";
pub const CLIENT_NAME: &str = "loom-host";
const MAX_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinedOracle {
    pub url: String,
    pub host_id: String,
    pub host_key: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_id_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_secret_env: Option<String>,
}

impl JoinedOracle {
    pub fn load(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(ORACLE_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("this host has not joined an oracle; run `loom-host join <link>` first")
            }
            Err(err) => return Err(err).with_context(|| format!("cannot read {}", path.display())),
        };
        serde_json::from_slice(&bytes).with_context(|| format!("cannot parse {}", path.display()))
    }

    fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let path = data_dir.join(ORACLE_FILE);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn access(&self) -> anyhow::Result<Option<AccessToken>> {
        match (&self.access_id_env, &self.access_secret_env) {
            (Some(id), Some(secret)) => AccessToken::from_env(id, secret)
                .map(Some)
                .map_err(anyhow::Error::msg),
            _ => Ok(None),
        }
    }

    pub fn endpoint(&self) -> anyhow::Result<OracleEndpoint> {
        Ok(OracleEndpoint::from_parts(
            &self.url,
            &self.host_id,
            &self.host_key,
            self.access()?,
        )?)
    }
}

pub fn identity(data_dir: &Path) -> anyhow::Result<ClientIdentity> {
    let path = data_dir.join(IDENTITY_FILE);
    ClientIdentity::load_or_create(&path)
        .with_context(|| format!("cannot read the key {}", path.display()))
}

pub struct JoinOptions {
    pub link: String,
    pub name: String,
    pub access_id_env: Option<String>,
    pub access_secret_env: Option<String>,
    pub force: bool,
}

pub async fn join(data_dir: &Path, options: JoinOptions) -> anyhow::Result<JoinedOracle> {
    if data_dir.join(ORACLE_FILE).exists() && !options.force {
        bail!(
            "this host has joined an oracle already; pass --force to join again ({})",
            data_dir.join(ORACLE_FILE).display()
        );
    }
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("cannot make {}", data_dir.display()))?;
    let mut joined = JoinedOracle {
        url: String::new(),
        host_id: String::new(),
        host_key: String::new(),
        name: options.name,
        access_id_env: options.access_id_env,
        access_secret_env: options.access_secret_env,
    };
    let identity = identity(data_dir)?;
    let endpoint = pair_oracle(&options.link, &identity, &joined.name, joined.access()?)
        .await
        .context("cannot pair with the oracle")?;
    joined.url = endpoint.url.clone();
    joined.host_id = endpoint.host.to_string();
    joined.host_key = endpoint.host_key_text();
    joined.save(data_dir)?;
    Ok(joined)
}

#[derive(Clone)]
pub struct Host {
    pub config: HostConfig,
    pub oracle: JoinedOracle,
    pub identity: Arc<ClientIdentity>,
    pub driver: Arc<dyn Driver>,
    pub agents: Arc<dyn AgentSource>,
    pub http: loom_github::Http,
}

fn internal(err: anyhow::Error) -> OracleError {
    OracleError::new(ErrorCode::Internal, format!("{err:#}"))
}

fn to_value(value: impl Serialize) -> Result<Value, OracleError> {
    serde_json::to_value(value)
        .map_err(|err| OracleError::new(ErrorCode::Internal, err.to_string()))
}

async fn checks(driver: &dyn Driver, docker: bool) -> Vec<HostCheck> {
    driver
        .check(docker)
        .await
        .into_iter()
        .map(|item| HostCheck {
            name: item.name,
            status: match item.status {
                CheckStatus::Ok => HostCheckStatus::Ok,
                CheckStatus::Warning => HostCheckStatus::Warning,
                CheckStatus::Failed => HostCheckStatus::Failed,
            },
            detail: item.detail,
        })
        .collect()
}

impl Host {
    async fn report(&self) -> anyhow::Result<HostReport> {
        let report = self.driver.report().await?;
        Ok(HostReport {
            version: loom_version::VERSION.to_owned(),
            driver: report.driver,
            cpus: report.cpus,
            memory_mib: report.memory_mib,
            architecture: report.architecture,
            storage_driver: report.storage_driver,
            incus_version: report.incus_version,
            max_codespaces: self.config.rules.max_codespaces,
            connection_types: vec![CONNECTION_RELAY.to_owned()],
            checks: checks(self.driver.as_ref(), false).await,
            git_proxy: self
                .config
                .git_proxy
                .as_ref()
                .map(|proxy| proxy.public_url()),
        })
    }

    async fn send_instances(&self, client: &OracleClient) {
        match self.driver.list(&self.oracle.host_id).await {
            Ok(instances) => {
                if let Err(err) = client.host_instances(instances).await {
                    tracing::warn!(error = %err, "cannot send the instances");
                }
            }
            Err(err) => tracing::warn!(error = %format!("{err:#}"), "cannot list the instances"),
        }
    }

    async fn create(
        &self,
        client: &OracleClient,
        request: &CreateInstance,
    ) -> Result<Value, OracleError> {
        if let Some(max) = self.config.rules.max_codespaces {
            let count = self
                .driver
                .list(&self.oracle.host_id)
                .await
                .map_err(internal)?
                .len();
            if count >= max as usize {
                return Err(OracleError::new(
                    ErrorCode::Conflict,
                    format!("the host has its maximum of {max} codespaces"),
                ));
            }
        }
        let architecture = match self.driver.report().await {
            Ok(report) => report.architecture,
            Err(err) => return Err(internal(err)),
        };
        let agent = self
            .agents
            .agent(request.agent_version.as_deref(), architecture.as_deref())
            .await
            .map_err(internal)?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let progress_client = client.clone();
        let codespace = request.codespace.clone();
        let forward = tokio::spawn(async move {
            while let Some(step) = rx.recv().await {
                if let Err(err) = progress_client.instance_progress(&codespace, &step).await {
                    tracing::debug!(error = %err, "cannot send a step");
                }
            }
        });
        let result = self
            .driver
            .create(request, &self.oracle.host_id, &agent, &Progress::new(tx))
            .await;
        let _ = forward.await;
        match result {
            Ok(()) => {
                tracing::info!(codespace = %request.codespace, instance = %request.instance, "created a codespace instance");
                Ok(Value::Object(Default::default()))
            }
            Err(err) => {
                tracing::warn!(codespace = %request.codespace, error = %format!("{err:#}"), "cannot create a codespace instance");
                if let Err(cleanup) = self
                    .driver
                    .delete(&self.oracle.host_id, &request.instance)
                    .await
                {
                    tracing::warn!(error = %format!("{cleanup:#}"), "cannot remove the failed instance");
                }
                Err(internal(err))
            }
        }
    }

    async fn handle(&self, client: &OracleClient, call: HostCall) -> Result<Value, OracleError> {
        match call {
            HostCall::CreateInstance(request) => self.create(client, &request).await,
            HostCall::DeleteInstance { instance, .. } => {
                self.driver
                    .delete(&self.oracle.host_id, &instance)
                    .await
                    .map_err(internal)?;
                tracing::info!(%instance, "deleted a codespace instance");
                Ok(Value::Object(Default::default()))
            }
            HostCall::StopInstance { instance, .. } => {
                tracing::info!(%instance, "stopping a codespace instance");
                self.driver
                    .stop(&self.oracle.host_id, &instance)
                    .await
                    .map_err(internal)?;
                tracing::info!(%instance, "stopped a codespace instance");
                Ok(Value::Object(Default::default()))
            }
            HostCall::StartInstance { instance, .. } => {
                tracing::info!(%instance, "starting a codespace instance");
                self.driver
                    .start(&self.oracle.host_id, &instance)
                    .await
                    .map_err(internal)?;
                Ok(Value::Object(Default::default()))
            }
            HostCall::ListInstances => {
                let instances = self
                    .driver
                    .list(&self.oracle.host_id)
                    .await
                    .map_err(internal)?;
                to_value(Instances { instances })
            }
            HostCall::CheckWorkspace(check) => {
                let mut result = crate::providers::check(&self.http, &check).await;
                result.volumes = true;
                result.directories = crate::volumes::check_directories(
                    &self.config.rules,
                    self.driver.local(),
                    &check.directories,
                );
                to_value(result)
            }
            HostCall::ProviderRequest(request) => {
                crate::providers::request(&self.http, &request).await
            }
            HostCall::DeleteVolume { volume } => {
                self.driver
                    .delete_volume(&self.oracle.host_id, &volume)
                    .await
                    .map_err(|err| {
                        let message = format!("{err:#}");
                        let code = if message.contains("in use") {
                            ErrorCode::Conflict
                        } else {
                            ErrorCode::Internal
                        };
                        OracleError::new(code, message)
                    })?;
                tracing::info!(%volume, "deleted a volume");
                Ok(Value::Object(Default::default()))
            }
            HostCall::Check { docker } => {
                tracing::info!(docker, "checking the host for the oracle");
                to_value(HostChecks {
                    checks: checks(self.driver.as_ref(), docker).await,
                })
            }
            HostCall::VolumeUsage { volumes } => to_value(VolumeUsages {
                volumes: self
                    .driver
                    .volume_usage(&self.oracle.host_id, &volumes)
                    .await,
            }),
            HostCall::Unknown => Err(OracleError::new(
                ErrorCode::UnknownCommand,
                "this host does not know the call; update loom-host",
            )),
        }
    }

    async fn session(
        &self,
        client: OracleClient,
        mut calls: OracleCalls,
        shutdown: &CancellationToken,
    ) {
        match self.report().await {
            Ok(report) => {
                if let Err(err) = client.host_report(report).await {
                    tracing::warn!(error = %err, "cannot send the report");
                }
            }
            Err(err) => tracing::error!(error = %format!("{err:#}"), "cannot make the report"),
        }
        self.send_instances(&client).await;
        let mut interval = tokio::time::interval(self.config.report_interval.as_duration());
        interval.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                call = calls.recv() => {
                    let Some((id, call)) = call else { return };
                    let host = self.clone();
                    let client = client.clone();
                    tokio::spawn(async move {
                        let result = host.handle(&client, call).await;
                        if let Err(err) = client.reply(id, result).await {
                            tracing::warn!(error = %err, "cannot reply to the oracle");
                        }
                    });
                }
                _ = interval.tick() => {
                    let host = self.clone();
                    let client = client.clone();
                    tokio::spawn(async move { host.send_instances(&client).await });
                }
            }
        }
    }

    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let endpoint = self.oracle.endpoint()?;
        let (current, client_watch) = tokio::sync::watch::channel(None);
        if let Some(config) = &self.config.git_proxy {
            let listen = config
                .listen
                .parse()
                .with_context(|| format!("{:?} is not an address", config.listen))?;
            let backend = crate::git_proxy::HostBackend {
                driver: self.driver.clone(),
                oracle: self.oracle.host_id.clone(),
                client: client_watch,
            };
            let proxy = Arc::new(crate::git_proxy::GitProxy::new(Arc::new(backend))?);
            tokio::spawn(crate::git_proxy::run(proxy, listen, shutdown.clone()));
        }
        let mut backoff = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
            match OracleClient::connect_with_calls(&endpoint, &self.identity, CLIENT_NAME).await {
                Ok((client, _events, calls)) => {
                    tracing::info!(oracle = %endpoint.url, "connected to the oracle");
                    backoff = Duration::from_secs(1);
                    current.send_replace(Some(client.clone()));
                    self.session(client, calls, &shutdown).await;
                    current.send_replace(None);
                    if shutdown.is_cancelled() {
                        break;
                    }
                    tracing::warn!(oracle = %endpoint.url, "lost the connection to the oracle");
                }
                Err(err) => {
                    tracing::warn!(oracle = %endpoint.url, error = %err, "cannot connect to the oracle")
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        Ok(())
    }
}
