use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use loom_agent_core::HostKeys;
use loom_config::AgentConfig;
use loom_crypto::{AuthorizedClients, CryptoError, FileResponder, HostIdentity};
use loom_proto::{ApiError, AuthorizedClient, ErrorCode, HostKeyInfo};
use loom_transport::{
    AccessToken, AgentLinkOptions, IncomingChannel, LinkState, RelayEndpoint, spawn_agent_link,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::server::{ServerContext, serve_stream};

const REVOCATION_POLL: Duration = Duration::from_secs(2);
pub const REGISTRATION_WAIT: Duration = Duration::from_secs(10);

type ClientSet = Arc<HashSet<[u8; 32]>>;

fn internal(err: CryptoError) -> ApiError {
    ApiError::new(ErrorCode::Internal, err.to_string())
}

pub struct LocalHostKeys {
    pub identity: Arc<HostIdentity>,
    pub clients: AuthorizedClients,
    pub relay_url: Option<String>,
    pub changed: watch::Sender<ClientSet>,
}

impl LocalHostKeys {
    pub fn new(identity: Arc<HostIdentity>, data_dir: &Path, config: &AgentConfig) -> Self {
        let clients = AuthorizedClients::in_dir(data_dir);
        let (changed, _) = watch::channel(current_set(&clients));
        Self {
            identity,
            clients,
            relay_url: config.relay.url.clone(),
            changed,
        }
    }

    fn info(&self) -> HostKeyInfo {
        HostKeyInfo {
            host: self.identity.host_id(),
            static_pubkey: self.identity.static_public(),
            relay_url: self.relay_url.clone(),
        }
    }

    fn refresh(&self) {
        let set = current_set(&self.clients);
        self.changed.send_if_modified(|old| {
            if **old == *set {
                false
            } else {
                *old = set;
                true
            }
        });
    }

    pub fn spawn_watcher(self: &Arc<Self>, shutdown: CancellationToken) {
        let keys = self.clone();
        tokio::spawn(async move {
            let mut last: Option<SystemTime> = keys.clients.modified();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(REVOCATION_POLL) => {}
                }
                let modified = keys.clients.modified();
                if modified != last {
                    last = modified;
                    keys.refresh();
                }
            }
        });
    }
}

fn current_set(clients: &AuthorizedClients) -> ClientSet {
    match clients.list() {
        Ok(list) => Arc::new(list.into_iter().map(|c| c.pubkey).collect()),
        Err(err) => {
            tracing::error!(error = %err, "cannot read the authorized client list");
            Arc::new(HashSet::new())
        }
    }
}

#[async_trait]
impl HostKeys for LocalHostKeys {
    async fn host_key(&self) -> Result<HostKeyInfo, ApiError> {
        Ok(self.info())
    }

    async fn authorize(&self, pubkey: [u8; 32], label: String) -> Result<HostKeyInfo, ApiError> {
        loom_crypto::validate_label(&label)
            .map_err(|e| ApiError::new(ErrorCode::BadRequest, e.to_string()))?;
        self.clients.authorize(pubkey, &label).map_err(internal)?;
        self.refresh();
        tracing::info!(%label, "authorized a relay client");
        Ok(self.info())
    }

    async fn list(&self) -> Result<Vec<AuthorizedClient>, ApiError> {
        Ok(self
            .clients
            .list()
            .map_err(internal)?
            .into_iter()
            .map(|c| AuthorizedClient {
                label: c.label,
                pubkey: c.pubkey,
                added_at: c.added_at,
            })
            .collect())
    }

    async fn revoke(&self, label: String) -> Result<(), ApiError> {
        if !self.clients.revoke(&label).map_err(internal)? {
            return Err(ApiError::new(
                ErrorCode::NotFound,
                format!("no client is labeled {label}"),
            ));
        }
        self.refresh();
        tracing::info!(%label, "revoked a relay client");
        Ok(())
    }
}

pub fn relay_endpoint(config: &AgentConfig) -> anyhow::Result<Option<RelayEndpoint>> {
    if !config.relay.enabled {
        return Ok(None);
    }
    let Some(url) = config.relay.url.clone() else {
        return Ok(None);
    };
    let access = match &config.relay.cloudflare_access {
        Some(access) => Some(
            AccessToken::from_env(&access.client_id_env, &access.client_secret_env)
                .map_err(anyhow::Error::msg)?,
        ),
        None => None,
    };
    Ok(Some(RelayEndpoint { url, access }))
}

pub struct RelayHandle {
    pub state: watch::Receiver<LinkState>,
}

pub fn start_relay(
    ctx: ServerContext,
    endpoint: RelayEndpoint,
    keys: Arc<LocalHostKeys>,
    data_dir: &Path,
    shutdown: CancellationToken,
) -> RelayHandle {
    let (tx, rx) = mpsc::channel::<IncomingChannel>(16);
    let (_task, state) = spawn_agent_link(
        AgentLinkOptions {
            endpoint,
            instance: ctx.info.instance.clone(),
            host: keys.identity.clone(),
            responder: Arc::new(FileResponder::in_dir(data_dir)),
        },
        tx,
        shutdown.clone(),
    );
    keys.spawn_watcher(shutdown.clone());
    tokio::spawn(accept_channels(ctx, rx, keys, shutdown));
    RelayHandle { state }
}

async fn accept_channels(
    ctx: ServerContext,
    mut incoming: mpsc::Receiver<IncomingChannel>,
    keys: Arc<LocalHostKeys>,
    shutdown: CancellationToken,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            channel = incoming.recv() => {
                let Some(channel) = channel else { break };
                tracing::info!(client = %channel.peer.label, channel = channel.channel, "relay client connected");
                let ctx = ctx.clone();
                let keys = keys.clone();
                let mut clients = keys.changed.subscribe();
                let pubkey = channel.peer.pubkey;
                let label = channel.peer.label.clone();
                connections.spawn(async move {
                    let revoked = async {
                        loop {
                            if !clients.borrow_and_update().contains(&pubkey) {
                                keys.refresh();
                                if !keys.changed.borrow().contains(&pubkey) {
                                    return;
                                }
                            }
                            if clients.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                    };
                    tokio::select! {
                        _ = serve_stream(ctx, channel.stream) => {}
                        _ = revoked => tracing::info!(client = %label, "closed a revoked client's channel"),
                    }
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
}

pub async fn wait_registered(handle: &mut RelayHandle) -> bool {
    tokio::time::timeout(
        REGISTRATION_WAIT,
        handle.state.wait_for(|s| *s == LinkState::Registered),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}
