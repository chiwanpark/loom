use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use loom_client::oracle_proto::{
    AgentAddress, Bootstrap, Bundle, DeviceKey, Event, repository_directory, valid_directory,
};
use loom_client::{ClientIdentity, OracleClient, OracleEndpoint, OracleEvents, pair_oracle};
use loom_config::{AgentConfig, Dirs};
use loom_crypto::{AuthorizedClients, HostIdentity, decode_key, encode_key};
use loom_transport::{AccessToken, LinkState};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::serve::{ServeOptions, Served, Started, serve};

pub const BOOTSTRAP_FILE: &str = "bootstrap.json";
pub const MANAGED_FILE: &str = "managed.json";
pub const ORACLE_IDENTITY_FILE: &str = "oracle_identity.json";
pub const DEFAULT_WORKSPACES_ROOT: &str = "/workspaces";
const CLIENT_NAME: &str = "loom-agent";
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const GIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedState {
    pub url: String,
    pub host_id: String,
    pub host_key: String,
    pub codespace: String,
    pub workspaces_root: PathBuf,
}

impl ManagedState {
    pub fn load(data_dir: &Path) -> anyhow::Result<Option<Self>> {
        let path = data_dir.join(MANAGED_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => {
                Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("cannot parse {}", path.display())
                })?))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, data_dir: &Path) -> anyhow::Result<()> {
        let path = data_dir.join(MANAGED_FILE);
        let partial = data_dir.join(format!("{MANAGED_FILE}.partial"));
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&partial)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            std::fs::rename(&partial, &path)
        };
        write().with_context(|| format!("cannot write {}", path.display()))
    }

    fn endpoint(&self, access: Option<AccessToken>) -> anyhow::Result<OracleEndpoint> {
        Ok(OracleEndpoint::from_parts(
            &self.url,
            &self.host_id,
            &self.host_key,
            access,
        )?)
    }
}

pub struct ManagedOptions {
    pub bootstrap: Option<PathBuf>,
    pub config_path: PathBuf,
    pub config: AgentConfig,
    pub dirs: Dirs,
}

pub fn default_bootstrap(dirs: &Dirs) -> PathBuf {
    dirs.config_dir().join(BOOTSTRAP_FILE)
}

fn access_token(config: &AgentConfig) -> anyhow::Result<Option<AccessToken>> {
    match &config.relay.cloudflare_access {
        Some(access) => Ok(Some(
            AccessToken::from_env(&access.client_id_env, &access.client_secret_env)
                .map_err(anyhow::Error::msg)?,
        )),
        None => Ok(None),
    }
}

async fn take_bootstrap(
    path: &Path,
    identity: &ClientIdentity,
    access: Option<AccessToken>,
    data_dir: &Path,
) -> anyhow::Result<ManagedState> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "this codespace agent has no state and no bootstrap file {}",
            path.display()
        )
    })?;
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes)
        .with_context(|| format!("cannot parse {}", path.display()))?;
    let endpoint = pair_oracle(&bootstrap.link, identity, &bootstrap.codespace, access)
        .await
        .context("cannot pair with the oracle")?;
    let state = ManagedState {
        url: endpoint.url.clone(),
        host_id: endpoint.host.to_string(),
        host_key: endpoint.host_key_text(),
        codespace: bootstrap.codespace,
        workspaces_root: bootstrap
            .workspaces_root
            .unwrap_or_else(|| PathBuf::from(DEFAULT_WORKSPACES_ROOT)),
    };
    state.save(data_dir)?;
    if let Err(err) = std::fs::remove_file(path) {
        tracing::warn!(error = %err, path = %path.display(), "cannot delete the bootstrap file");
    }
    tracing::info!(codespace = %state.codespace, oracle = %state.url, "paired with the oracle");
    Ok(state)
}

async fn connect(
    endpoint: &OracleEndpoint,
    identity: &ClientIdentity,
) -> (OracleClient, OracleEvents) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match OracleClient::connect(endpoint, identity, CLIENT_NAME).await {
            Ok(connected) => return connected,
            Err(err) => {
                tracing::warn!(error = %err, oracle = %endpoint.url, "cannot connect to the oracle")
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

pub fn sync_devices(data_dir: &Path, devices: &[DeviceKey]) -> anyhow::Result<bool> {
    let clients = AuthorizedClients::in_dir(data_dir);
    let wanted: Vec<([u8; 32], &str)> = devices
        .iter()
        .filter_map(|device| Some((decode_key(&device.pubkey)?, device.label.as_str())))
        .collect();
    let current = clients.list()?;
    let keep: HashSet<([u8; 32], &str)> = wanted.iter().copied().collect();
    let mut changed = false;
    for client in &current {
        if !keep.contains(&(client.pubkey, client.label.as_str())) {
            clients.revoke(&client.label)?;
            changed = true;
        }
    }
    let present: HashSet<([u8; 32], &str)> = current
        .iter()
        .map(|client| (client.pubkey, client.label.as_str()))
        .collect();
    for (pubkey, label) in wanted {
        if !present.contains(&(pubkey, label)) {
            clients.authorize(pubkey, label)?;
            changed = true;
        }
    }
    Ok(changed)
}

async fn git(args: &[&str], dir: Option<&Path>) -> anyhow::Result<()> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {} took too long", args[0]))?
        .context("cannot run git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args[0], stderr.trim());
    }
    Ok(())
}

pub async fn prepare_workspace(root: &Path, bundle: &Bundle) -> anyhow::Result<PathBuf> {
    let repository = bundle
        .repositories
        .first()
        .context("the workspace has no repository")?;
    let directory = repository
        .directory
        .clone()
        .or_else(|| repository_directory(&repository.url))
        .filter(|directory| valid_directory(directory))
        .context("the repository has no valid directory name")?;
    let path = root.join(&directory);
    if path.join(".git").exists() {
        return Ok(path);
    }
    if path.exists() {
        bail!("{} is there already, but it is not a clone", path.display());
    }
    std::fs::create_dir_all(root).with_context(|| format!("cannot make {}", root.display()))?;
    let partial = root.join(format!(".{directory}.partial"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)
            .with_context(|| format!("cannot remove {}", partial.display()))?;
    }
    let partial_text = partial.to_string_lossy().into_owned();
    let mut args = vec!["clone", "--quiet"];
    if let Some(branch) = &repository.base_branch {
        args.extend(["--branch", branch]);
    }
    args.extend(["--", &repository.url, &partial_text]);
    git(&args, None).await?;
    git(&["switch", "--quiet", "-c", &bundle.branch], Some(&partial)).await?;
    std::fs::rename(&partial, &path)
        .with_context(|| format!("cannot move the clone to {}", path.display()))?;
    Ok(path)
}

async fn refresh_devices(client: &OracleClient, data_dir: &Path) {
    match client.bundle().await {
        Ok(bundle) => match sync_devices(data_dir, &bundle.devices) {
            Ok(true) => tracing::info!(devices = bundle.devices.len(), "updated the devices"),
            Ok(false) => {}
            Err(err) => tracing::error!(error = %err, "cannot write the devices"),
        },
        Err(err) => tracing::warn!(error = %err, "cannot read the devices from the oracle"),
    }
}

struct Link {
    endpoint: OracleEndpoint,
    identity: ClientIdentity,
    data_dir: PathBuf,
    address: AgentAddress,
}

impl Link {
    async fn run(
        mut self,
        mut client: OracleClient,
        mut events: OracleEvents,
        started: oneshot::Receiver<Started>,
    ) {
        let Ok(started) = started.await else {
            return;
        };
        self.address.instance = Some(started.instance.0.clone());
        if let Some(mut relay) = started.relay
            && relay
                .wait_for(|s| *s == LinkState::Registered)
                .await
                .is_err()
        {
            return;
        }
        loop {
            let reported = async {
                client.codespace_agent(self.address.clone()).await?;
                client.codespace_ready().await
            };
            match reported.await {
                Ok(()) => tracing::info!("told the oracle that the codespace is ready"),
                Err(err) => tracing::warn!(error = %err, "cannot report to the oracle"),
            }
            while let Some(event) = events.recv().await {
                if matches!(event, Event::DeviceKeysChanged | Event::EventsLost) {
                    refresh_devices(&client, &self.data_dir).await;
                }
            }
            tracing::warn!("lost the connection to the oracle");
            tokio::time::sleep(Duration::from_secs(1)).await;
            (client, events) = connect(&self.endpoint, &self.identity).await;
            if let Err(err) = client.subscribe().await {
                tracing::warn!(error = %err, "cannot subscribe to the oracle");
            }
            refresh_devices(&client, &self.data_dir).await;
        }
    }
}

pub async fn serve_managed(options: ManagedOptions) -> anyhow::Result<Served> {
    let ManagedOptions {
        bootstrap,
        config_path,
        mut config,
        dirs,
    } = options;
    let data_dir = loom_agent_core::data_dir(&config, &dirs);
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot make {}", data_dir.display()))?;
    let identity = ClientIdentity::load_or_create(&data_dir.join(ORACLE_IDENTITY_FILE))
        .context("cannot load the key for the oracle")?;
    let access = access_token(&config)?;
    let state = match ManagedState::load(&data_dir)? {
        Some(state) => state,
        None => {
            let path = bootstrap.unwrap_or_else(|| default_bootstrap(&dirs));
            take_bootstrap(&path, &identity, access.clone(), &data_dir).await?
        }
    };
    let endpoint = state.endpoint(access)?;
    let host = HostIdentity::load_or_create(&data_dir).context("cannot load the host keys")?;
    let address = AgentAddress {
        host_id: host.host_id().to_string(),
        host_key: encode_key(&host.static_public()),
        instance: None,
    };
    let (client, events) = connect(&endpoint, &identity).await;
    client
        .codespace_agent(address.clone())
        .await
        .context("the oracle refused the keys of this agent")?;
    client.subscribe().await?;
    let bundle = client.bundle().await.context("cannot get the bundle")?;
    sync_devices(&data_dir, &bundle.devices)?;
    let _ = client.codespace_progress("clone").await;
    let workspace = match prepare_workspace(&state.workspaces_root, &bundle).await {
        Ok(workspace) => workspace,
        Err(err) => {
            let message = format!("{err:#}");
            tracing::error!(error = %message, "cannot prepare the workspace");
            client.codespace_failed(&message).await?;
            return Ok(Served::Exit(0));
        }
    };
    let _ = client.codespace_progress("agent").await;
    config.relay.enabled = true;
    config.relay.url = Some(bundle.relay_url.clone());
    config.allowed_roots = vec![state.workspaces_root.clone()];
    config.lifecycle.default_workspace = Some(workspace.clone());
    let (ready, started) = oneshot::channel();
    let link = Link {
        endpoint,
        identity,
        data_dir,
        address,
    };
    let task = tokio::spawn(link.run(client, events, started));
    let served = serve(ServeOptions {
        workspace: Some(workspace),
        config_path,
        config,
        dirs,
        ready: Some(ready),
    })
    .await;
    task.abort();
    served
}

#[cfg(test)]
mod tests {
    use loom_client::oracle_proto::Repository;

    use super::*;

    fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn device(label: &str, byte: u8) -> DeviceKey {
        DeviceKey {
            label: label.into(),
            pubkey: encode_key(&key(byte)),
        }
    }

    #[test]
    fn the_devices_of_the_oracle_become_the_authorized_clients() {
        let dir = tempfile::tempdir().unwrap();
        let clients = AuthorizedClients::in_dir(dir.path());
        clients.authorize(key(9), "old").unwrap();
        assert!(sync_devices(dir.path(), &[device("laptop", 1), device("phone", 2)]).unwrap());
        clients.set_push_token(&key(2), Some("token")).unwrap();
        assert!(!sync_devices(dir.path(), &[device("laptop", 1), device("phone", 2)]).unwrap());
        let list = clients.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(
            list.iter()
                .any(|c| c.label == "phone" && c.push_token.is_some())
        );
        assert!(sync_devices(dir.path(), &[device("phone", 2)]).unwrap());
        let list = clients.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pubkey, key(2));
    }

    #[test]
    fn the_state_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ManagedState::load(dir.path()).unwrap().is_none());
        let state = ManagedState {
            url: "wss://oracle.example.com".into(),
            host_id: "ab".repeat(32),
            host_key: encode_key(&key(3)),
            codespace: "c1".into(),
            workspaces_root: "/workspaces".into(),
        };
        state.save(dir.path()).unwrap();
        assert_eq!(ManagedState::load(dir.path()).unwrap(), Some(state));
    }

    async fn run(dir: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[tokio::test]
    async fn the_workspace_is_a_clone_on_the_branch_of_the_codespace() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "--quiet", "-b", "main"]).await;
        std::fs::write(origin.join("README.md"), "hi\n").unwrap();
        run(&origin, &["add", "README.md"]).await;
        run(&origin, &["commit", "--quiet", "-m", "first"]).await;
        let root = dir.path().join("workspaces");
        let bundle = Bundle {
            codespace: "c1".into(),
            workspace: "acme".into(),
            branch: "loom/c1".into(),
            repositories: vec![Repository {
                url: origin.to_string_lossy().into_owned(),
                provider: None,
                directory: Some("server".into()),
                base_branch: Some("main".into()),
            }],
            devices: Vec::new(),
            relay_url: "ws://127.0.0.1:1".into(),
            preset: None,
            git: Default::default(),
        };
        let path = prepare_workspace(&root, &bundle).await.unwrap();
        assert_eq!(path, root.join("server"));
        assert!(path.join("README.md").exists());
        let head = std::fs::read_to_string(path.join(".git/HEAD")).unwrap();
        assert_eq!(head.trim(), "ref: refs/heads/loom/c1");
        assert_eq!(prepare_workspace(&root, &bundle).await.unwrap(), path);

        let mut missing = bundle.clone();
        missing.repositories[0].directory = Some("other".into());
        missing.repositories[0].url = dir.path().join("nothing").to_string_lossy().into_owned();
        let err = prepare_workspace(&root, &missing).await.unwrap_err();
        assert!(format!("{err:#}").contains("git clone failed"), "{err:#}");
        assert!(!root.join("other").exists());
    }
}
