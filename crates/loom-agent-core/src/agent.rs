use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use loom_config::{AgentConfig, Dirs};
use loom_llm::{Auth, CredentialStore, HttpClient, LlmProvider, ModelRef, http_client};
use loom_proto::{SessionId, SessionInfo};
use loom_tools::Workspace;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::{CoreError, Result};
use crate::runtime::{Runtime, RuntimeCell};
use crate::session::{Session, SessionParams};
use crate::store::{EventStore, store_path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentNotice {
    ConfigReloaded { restart_required: Vec<String> },
    ConfigRejected { message: String },
    ShuttingDown,
}

pub struct AgentOptions {
    pub workspace: PathBuf,
    pub config: AgentConfig,
    pub dirs: Dirs,
    pub http: Option<HttpClient>,
    pub providers: Vec<Arc<dyn LlmProvider>>,
    pub persist: bool,
}

impl AgentOptions {
    pub fn new(workspace: impl Into<PathBuf>, config: AgentConfig, dirs: Dirs) -> Self {
        Self {
            workspace: workspace.into(),
            config,
            dirs,
            http: None,
            providers: Vec::new(),
            persist: true,
        }
    }
}

struct Inner {
    workspace: Arc<Workspace>,
    dirs: Dirs,
    auth: Auth,
    overrides: Vec<Arc<dyn LlmProvider>>,
    runtime: RuntimeCell,
    sessions: Mutex<BTreeMap<SessionId, Session>>,
    notices: broadcast::Sender<AgentNotice>,
    store: Option<EventStore>,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

pub fn data_dir(config: &AgentConfig, dirs: &Dirs) -> PathBuf {
    config
        .storage
        .data_dir
        .clone()
        .unwrap_or_else(|| dirs.default_data_dir())
}

fn restart_required(current: &AgentConfig, next: &AgentConfig) -> Vec<String> {
    let mut changed = Vec::new();
    if current.relay != next.relay {
        changed.push("relay");
    }
    if current.socket != next.socket {
        changed.push("socket");
    }
    if current.storage != next.storage {
        changed.push("storage");
    }
    if current.lifecycle != next.lifecycle {
        changed.push("lifecycle");
    }
    changed.into_iter().map(str::to_owned).collect()
}

impl Agent {
    pub fn new(options: AgentOptions) -> Result<Self> {
        let workspace = Arc::new(Workspace::new(&options.workspace)?);
        let http = options.http.unwrap_or_else(http_client);
        let store = CredentialStore::in_dir(&data_dir(&options.config, &options.dirs));
        let auth = Auth::new(store, http);
        let data = data_dir(&options.config, &options.dirs);
        let runtime = Runtime::build(options.config, &auth, &options.providers)?;
        let (notices, _) = broadcast::channel(64);
        let (store, stored) = if options.persist {
            let path = store_path(&data, workspace.root());
            match EventStore::open(&path, workspace.root()) {
                Ok((store, stored)) => (Some(store), stored),
                Err(err) => {
                    tracing::error!(error = %err, "sessions will not be saved");
                    (None, Vec::new())
                }
            }
        } else {
            (None, Vec::new())
        };
        let agent = Self {
            inner: Arc::new(Inner {
                workspace,
                dirs: options.dirs,
                auth,
                overrides: options.providers,
                runtime: RuntimeCell::new(runtime),
                sessions: Mutex::new(BTreeMap::new()),
                notices,
                store,
            }),
        };
        for stored in stored {
            let session = Session::restore(agent.session_params(None), stored);
            agent.insert(session);
        }
        Ok(agent)
    }

    fn session_params(&self, model: Option<ModelRef>) -> SessionParams {
        SessionParams {
            runtime: self.inner.runtime.clone(),
            workspace: self.inner.workspace.clone(),
            system_agents_md: self.inner.dirs.system_agents_md(),
            model,
            store: self.inner.store.clone(),
        }
    }

    fn insert(&self, session: Session) {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session.id().clone(), session);
    }

    pub fn workspace_root(&self) -> &Path {
        self.inner.workspace.root()
    }

    pub fn auth(&self) -> &Auth {
        &self.inner.auth
    }

    pub fn config(&self) -> Arc<AgentConfig> {
        self.inner.runtime.current().config.clone()
    }

    pub fn runtime(&self) -> Arc<Runtime> {
        self.inner.runtime.current()
    }

    pub fn notices(&self) -> broadcast::Receiver<AgentNotice> {
        self.inner.notices.subscribe()
    }

    pub(crate) fn notify(&self, notice: AgentNotice) {
        let _ = self.inner.notices.send(notice);
    }

    pub fn apply_config(&self, mut next: AgentConfig) -> Result<Vec<String>> {
        let current = self.config();
        let restart = restart_required(&current, &next);
        next.relay = current.relay.clone();
        next.socket = current.socket.clone();
        next.storage = current.storage.clone();
        next.lifecycle = current.lifecycle.clone();
        let runtime = Runtime::build(next, &self.inner.auth, &self.inner.overrides)?;
        self.inner.runtime.replace(runtime);
        self.notify(AgentNotice::ConfigReloaded {
            restart_required: restart.clone(),
        });
        Ok(restart)
    }

    pub fn create_session(&self, model: Option<&str>) -> Result<Session> {
        let runtime = self.inner.runtime.current();
        let model = model.map(|m| runtime.parse_model(m)).transpose()?;
        let session = Session::new(self.session_params(model));
        self.insert(session.clone());
        Ok(session)
    }

    pub fn session(&self, id: &SessionId) -> Result<Session> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| CoreError::UnknownSession(id.0.clone()))
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<SessionInfo> = self.sessions().iter().map(Session::info).collect();
        sessions.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        sessions
    }

    fn sessions(&self) -> Vec<Session> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub async fn shutdown(&self) {
        self.notify(AgentNotice::ShuttingDown);
        let sessions = self.sessions();
        for session in &sessions {
            session.interrupt();
        }
        for session in &sessions {
            session.wait_idle().await;
        }
        self.flush().await;
    }

    pub async fn flush(&self) {
        if let Some(store) = &self.inner.store {
            store.flush().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_restart_only_changes() {
        let current = AgentConfig::default();
        let mut next = current.clone();
        next.policy.default = loom_config::PolicyAction::Allow;
        assert!(restart_required(&current, &next).is_empty());
        next.socket.dir = Some(PathBuf::from("/tmp/x"));
        next.lifecycle.default_workspace = Some(PathBuf::from("/tmp"));
        assert_eq!(restart_required(&current, &next), ["socket", "lifecycle"]);
    }
}
