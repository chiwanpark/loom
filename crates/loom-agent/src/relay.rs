use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use loom_config::AgentConfig;
use loom_crypto::{AuthorizedClients, FileResponder, HostIdentity};
use loom_transport::{
    AccessToken, AgentLinkOptions, IncomingChannel, LinkState, RelayEndpoint, spawn_agent_link,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::push::{PushEvent, start_push};
use crate::server::{ServerContext, serve_stream};

const REVOCATION_POLL: Duration = Duration::from_secs(2);
pub const REGISTRATION_WAIT: Duration = Duration::from_secs(10);

type ClientSet = Arc<HashSet<[u8; 32]>>;

pub struct LocalHostKeys {
    pub identity: Arc<HostIdentity>,
    pub clients: AuthorizedClients,
    pub changed: watch::Sender<ClientSet>,
}

impl LocalHostKeys {
    pub fn new(identity: Arc<HostIdentity>, data_dir: &Path) -> Self {
        let clients = AuthorizedClients::in_dir(data_dir);
        let (changed, _) = watch::channel(current_set(&clients));
        Self {
            identity,
            clients,
            changed,
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

/// Links the agent to its relay. With `push`, the events of
/// [`crate::push::PushObserver`], it also sends notifications to the
/// clients that registered a push token.
pub fn start_relay(
    ctx: ServerContext,
    endpoint: RelayEndpoint,
    keys: Arc<LocalHostKeys>,
    data_dir: &Path,
    push: Option<mpsc::Receiver<PushEvent>>,
    shutdown: CancellationToken,
) -> RelayHandle {
    let (tx, rx) = mpsc::channel::<IncomingChannel>(16);
    let push = push.map(|events| {
        start_push(
            ctx.agent.clone(),
            ctx.info.clone(),
            keys.clone(),
            events,
            shutdown.clone(),
        )
    });
    let (_task, state) = spawn_agent_link(
        AgentLinkOptions {
            endpoint,
            instance: ctx.info.instance.clone(),
            host: keys.identity.clone(),
            responder: Arc::new(FileResponder::in_dir(data_dir)),
            push,
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
