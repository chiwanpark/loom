use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use loom_agent_core::{Agent, AgentOptions, watch_config};
use loom_config::{AgentConfig, Dirs};
use loom_crypto::HostIdentity;
use loom_proto::{AgentInfo, InstanceId};
use loom_transport::{AgentEntry, Registry, RegistryError, UnixFrameListener};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::relay::{LocalHostKeys, relay_endpoint, start_relay, wait_registered};
use crate::server::{ServerContext, run_listener};
use crate::spawn::EXIT_ALREADY_RUNNING;
use crate::supervisor::LocalSupervisor;
use crate::workspace::resolve_workspace;

pub struct ServeOptions {
    pub workspace: Option<PathBuf>,
    pub config_path: PathBuf,
    pub config: AgentConfig,
    pub dirs: Dirs,
}

pub fn new_instance_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub async fn serve(options: ServeOptions) -> anyhow::Result<i32> {
    let ServeOptions {
        workspace,
        config_path,
        config,
        dirs,
    } = options;
    let workspace = resolve_workspace(workspace.as_deref(), &config, &dirs)?;
    let instance = new_instance_id();
    let registry = Registry::new(config.runtime_dir(&dirs));
    let mut registration = match registry.claim(&instance, &workspace) {
        Ok(registration) => registration,
        Err(RegistryError::AlreadyRunning(path)) => {
            eprintln!("an agent is already running for {}", path.display());
            return Ok(EXIT_ALREADY_RUNNING);
        }
        Err(err) => return Err(err).context("could not register the agent"),
    };

    let data_dir = loom_agent_core::data_dir(&config, &dirs);
    let relay_endpoint = match relay_endpoint(&config) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            tracing::error!(error = %err, "the relay is disabled");
            None
        }
    };
    let identity = Arc::new(
        HostIdentity::load_or_create(&data_dir).context("could not load the host identity")?,
    );
    let host_keys = Arc::new(LocalHostKeys::new(identity, &data_dir, &config));
    let agent = Agent::new(AgentOptions::new(&workspace, config, dirs.clone()))
        .context("could not start the agent")?;
    let _watcher = start_watcher(&agent, &config_path, &dirs);

    let socket = registry.socket_path(&instance);
    let listener = UnixFrameListener::bind(&socket)
        .with_context(|| format!("could not listen on {}", socket.display()))?;
    let started_at = now_unix();
    let pid = std::process::id();
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let entry = AgentEntry {
        instance: instance.clone(),
        pid,
        workspace: workspace.clone(),
        socket: socket.clone(),
        started_at,
        version: version.clone(),
    };

    let info = AgentInfo {
        instance: InstanceId(instance.clone()),
        workspace: workspace.clone(),
        pid,
        started_at,
        version,
        default_model: None,
    };
    let shutdown = CancellationToken::new();
    let supervisor = LocalSupervisor {
        agent: agent.clone(),
        registry: registry.clone(),
        exe: std::env::current_exe().context("cannot locate the loom-agent binary")?,
        config_path,
        dirs,
        instance: info.instance.clone(),
        shutdown: shutdown.clone(),
    };
    let ctx = ServerContext {
        agent,
        info,
        supervisor: Some(Arc::new(supervisor)),
        host_keys: Some(host_keys.clone()),
    };
    if let Some(endpoint) = relay_endpoint {
        let url = endpoint.url.clone();
        let mut handle = start_relay(
            ctx.clone(),
            endpoint,
            host_keys,
            &data_dir,
            shutdown.clone(),
        );
        if !wait_registered(&mut handle).await {
            tracing::warn!(relay = %url, "not registered with the relay yet; continuing in the background");
        }
    }
    registration.publish(entry)?;
    tracing::info!(%instance, workspace = %workspace.display(), socket = %socket.display(), "agent ready");
    let signals = shutdown.clone();
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::spawn(async move {
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = int.recv() => tracing::info!("received SIGINT"),
        }
        signals.cancel();
    });
    run_listener(ctx, listener, shutdown).await;
    drop(registration);
    tracing::info!("agent stopped");
    Ok(0)
}

fn start_watcher(
    agent: &Agent,
    config_path: &Path,
    dirs: &Dirs,
) -> Option<loom_agent_core::ConfigWatcher> {
    if let Some(parent) = config_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match watch_config(agent.clone(), config_path.to_path_buf(), dirs.clone()) {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            tracing::warn!(error = %err, path = %config_path.display(), "config hot-reload is disabled");
            None
        }
    }
}
