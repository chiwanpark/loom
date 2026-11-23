use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use loom_client::oracle_proto::{
    AgentAddress, Bootstrap, Bundle, DeviceKey, Event, GitSetup, repository_directory,
    valid_directory,
};
use loom_client::{ClientIdentity, OracleClient, OracleEndpoint, OracleEvents, pair_oracle};
use loom_config::{AgentConfig, Dirs};
use loom_crypto::{AuthorizedClients, HostIdentity, decode_key, encode_key};
use loom_transport::{AccessToken, LinkState};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::codespace_git::{GitPaths, ManagedClone, apply_all, serve_credentials};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedClone {
    pub clone: ManagedClone,
    pub url: String,
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub root: PathBuf,
    pub clones: Vec<PlannedClone>,
}

impl Layout {
    pub fn managed(&self) -> Vec<ManagedClone> {
        self.clones.iter().map(|c| c.clone.clone()).collect()
    }
}

pub fn layout(workspaces_root: &Path, bundle: &Bundle) -> anyhow::Result<Layout> {
    if bundle.repositories.is_empty() {
        bail!("the workspace has no repository");
    }
    let root = if bundle.repositories.len() == 1 {
        None
    } else if valid_directory(&bundle.workspace) {
        Some(workspaces_root.join(&bundle.workspace))
    } else {
        bail!("{:?} is not a valid directory name", bundle.workspace);
    };
    let mut clones: Vec<PlannedClone> = Vec::new();
    for repository in &bundle.repositories {
        let directory = repository
            .directory
            .clone()
            .or_else(|| repository_directory(&repository.url))
            .filter(|directory| valid_directory(directory))
            .with_context(|| format!("{} has no valid directory name", repository.url))?;
        let path = root.as_deref().unwrap_or(workspaces_root).join(&directory);
        if clones.iter().any(|c| c.clone.path == path) {
            bail!("two repositories have the directory {directory}");
        }
        clones.push(PlannedClone {
            clone: ManagedClone {
                path,
                provider: repository.provider.clone(),
            },
            url: repository.url.clone(),
            base_branch: repository.base_branch.clone(),
        });
    }
    let root = root.unwrap_or_else(|| clones[0].clone.path.clone());
    Ok(Layout { root, clones })
}

async fn prepare_clone(planned: &PlannedClone, branch: &str) -> anyhow::Result<()> {
    let path = &planned.clone.path;
    if path.join(".git").exists() {
        return Ok(());
    }
    if path.exists() {
        bail!("{} is there already, but it is not a clone", path.display());
    }
    let parent = path.parent().context("a clone has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("a clone has no directory name")?;
    std::fs::create_dir_all(parent).with_context(|| format!("cannot make {}", parent.display()))?;
    let partial = parent.join(format!(".{name}.partial"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)
            .with_context(|| format!("cannot remove {}", partial.display()))?;
    }
    let partial_text = partial.to_string_lossy().into_owned();
    let mut args = vec!["clone", "--quiet"];
    if let Some(branch) = &planned.base_branch {
        args.extend(["--branch", branch]);
    }
    args.extend(["--", &planned.url, &partial_text]);
    git(&args, None)
        .await
        .with_context(|| format!("cannot clone {}", planned.url))?;
    git(&["switch", "--quiet", "-c", branch], Some(&partial)).await?;
    git(
        &["config", "--local", "push.autoSetupRemote", "true"],
        Some(&partial),
    )
    .await?;
    std::fs::rename(&partial, path)
        .with_context(|| format!("cannot move the clone to {}", path.display()))?;
    Ok(())
}

pub async fn prepare_clones(layout: &Layout, branch: &str) -> anyhow::Result<()> {
    for planned in &layout.clones {
        prepare_clone(planned, branch).await?;
    }
    Ok(())
}

struct Link {
    endpoint: OracleEndpoint,
    identity: ClientIdentity,
    data_dir: PathBuf,
    address: AgentAddress,
    git: GitPaths,
    clones: Vec<ManagedClone>,
    clients: watch::Sender<OracleClient>,
    setup: GitSetup,
}

impl Link {
    async fn refresh(&mut self, client: &OracleClient) {
        let bundle = match client.bundle().await {
            Ok(bundle) => bundle,
            Err(err) => {
                tracing::warn!(error = %err, "cannot read the bundle from the oracle");
                return;
            }
        };
        match sync_devices(&self.data_dir, &bundle.devices) {
            Ok(true) => tracing::info!(devices = bundle.devices.len(), "updated the devices"),
            Ok(false) => {}
            Err(err) => tracing::error!(error = %err, "cannot write the devices"),
        }
        if bundle.git != self.setup {
            match apply_all(&self.git, &bundle.git, &self.clones).await {
                Ok(()) => {
                    tracing::info!("updated the Git setup");
                    self.setup = bundle.git;
                }
                Err(err) => {
                    tracing::error!(error = %format!("{err:#}"), "cannot update the Git setup")
                }
            }
        }
    }

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
                if matches!(
                    event,
                    Event::DeviceKeysChanged | Event::GitChanged | Event::EventsLost
                ) {
                    self.refresh(&client).await;
                }
            }
            tracing::warn!("lost the connection to the oracle");
            tokio::time::sleep(Duration::from_secs(1)).await;
            (client, events) = connect(&self.endpoint, &self.identity).await;
            if let Err(err) = client.subscribe().await {
                tracing::warn!(error = %err, "cannot subscribe to the oracle");
            }
            self.clients.send_replace(client.clone());
            self.refresh(&client).await;
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
    let git = GitPaths::new(&data_dir, &dirs.config_home)?;
    let (clients, client_watch) = watch::channel(client.clone());
    let helper = serve_credentials(&git, client_watch)?;
    let layout = match layout(&state.workspaces_root, &bundle) {
        Ok(layout) => layout,
        Err(err) => {
            let message = format!("{err:#}");
            tracing::error!(error = %message, "cannot plan the clones");
            client.codespace_failed(&message).await?;
            return Ok(Served::Exit(0));
        }
    };
    let clones = layout.managed();
    let _ = client.codespace_progress("clone").await;
    let prepared = async {
        crate::codespace_git::apply(&git, &bundle.git)
            .await
            .context("cannot set up Git")?;
        prepare_clones(&layout, &bundle.branch).await?;
        apply_all(&git, &bundle.git, &clones)
            .await
            .context("cannot set up Git")
    };
    if let Err(err) = prepared.await {
        let message = format!("{err:#}");
        tracing::error!(error = %message, "cannot prepare the workspace");
        client.codespace_failed(&message).await?;
        helper.abort();
        return Ok(Served::Exit(0));
    }
    let workspace = layout.root.clone();
    let _ = client.codespace_progress("agent").await;
    config.relay.enabled = true;
    config.relay.url = Some(bundle.relay_url.clone());
    config.allowed_roots = vec![state.workspaces_root.clone()];
    config.lifecycle.default_workspace = Some(workspace.clone());
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("cannot make {}", workspace.display()))?;
    let (ready, started) = oneshot::channel();
    let link = Link {
        endpoint,
        identity,
        data_dir,
        address,
        git,
        clones,
        clients,
        setup: bundle.git.clone(),
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
    helper.abort();
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
    async fn each_repository_is_a_clone_on_the_branch_of_the_codespace() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        run(&origin, &["init", "--quiet", "-b", "main"]).await;
        std::fs::write(origin.join("README.md"), "hi\n").unwrap();
        run(&origin, &["add", "README.md"]).await;
        run(&origin, &["commit", "--quiet", "-m", "first"]).await;
        let root = dir.path().join("workspaces");
        let mut bundle = Bundle {
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
        let single = layout(&root, &bundle).unwrap();
        assert_eq!(single.root, root.join("server"));
        prepare_clones(&single, &bundle.branch).await.unwrap();
        let path = &single.root;
        assert!(path.join("README.md").exists());
        let head = std::fs::read_to_string(path.join(".git/HEAD")).unwrap();
        assert_eq!(head.trim(), "ref: refs/heads/loom/c1");
        prepare_clones(&single, &bundle.branch).await.unwrap();

        let mut second = bundle.repositories[0].clone();
        second.directory = Some("client".into());
        second.provider = Some("ghe".into());
        bundle.repositories.push(second);
        let double = layout(&root, &bundle).unwrap();
        assert_eq!(double.root, root.join("acme"));
        assert_eq!(
            double.managed(),
            vec![
                ManagedClone {
                    path: root.join("acme/server"),
                    provider: None,
                },
                ManagedClone {
                    path: root.join("acme/client"),
                    provider: Some("ghe".into()),
                },
            ]
        );
        prepare_clones(&double, &bundle.branch).await.unwrap();
        assert!(root.join("acme/client/README.md").exists());
        let mut same = bundle.clone();
        same.repositories[1].directory = Some("server".into());
        assert!(layout(&root, &same).is_err());

        let mut missing = bundle.clone();
        missing.repositories.truncate(1);
        missing.repositories[0].directory = Some("other".into());
        missing.repositories[0].url = dir.path().join("nothing").to_string_lossy().into_owned();
        let planned = layout(&root, &missing).unwrap();
        let err = prepare_clones(&planned, &missing.branch).await.unwrap_err();
        assert!(format!("{err:#}").contains("git clone failed"), "{err:#}");
        assert!(!root.join("other").exists());
    }
}
