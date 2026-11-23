use std::collections::HashMap;
use std::time::Duration;

use gpui::{AsyncApp, Context, EventEmitter, Task};
use loom_client::OracleClient;
use loom_client::oracle_proto::{
    Codespace, CodespaceSession, CodespaceState, Event, Host, Preset, Workspace,
};
use loom_config::HostEntry;
use loom_view_model::codespaces::is_listed;

use crate::backend::{Backend, next};
use crate::connection::{self, Env};

const RETRY_INITIAL: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleStatus {
    Connecting,
    Connected,
    Offline(String),
}

pub enum OracleEvent {
    Ready(Codespace),
    Failed(Codespace),
}

#[derive(Default)]
struct Lists {
    workspaces: Vec<Workspace>,
    codespaces: Vec<Codespace>,
    presets: Vec<Preset>,
    hosts: Vec<Host>,
    sessions: Vec<CodespaceSession>,
}

pub struct OracleLink {
    pub entry: HostEntry,
    env: Env,
    backend: Backend,
    pub status: OracleStatus,
    client: Option<OracleClient>,
    pub workspaces: Vec<Workspace>,
    pub codespaces: Vec<Codespace>,
    pub presets: Vec<Preset>,
    pub hosts: Vec<Host>,
    pub sessions: Vec<CodespaceSession>,
    pub progress: HashMap<String, Vec<String>>,
    generation: u64,
    retry: Duration,
    _pump: Option<Task<()>>,
}

impl EventEmitter<OracleEvent> for OracleLink {}

impl OracleLink {
    pub fn new(entry: HostEntry, env: Env, backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            entry,
            env,
            backend,
            status: OracleStatus::Connecting,
            client: None,
            workspaces: Vec::new(),
            codespaces: Vec::new(),
            presets: Vec::new(),
            hosts: Vec::new(),
            sessions: Vec::new(),
            progress: HashMap::new(),
            generation: 0,
            retry: RETRY_INITIAL,
            _pump: None,
        };
        this.connect(cx);
        this
    }

    pub fn name(&self) -> &str {
        self.entry.name()
    }

    pub fn client(&self) -> Option<OracleClient> {
        self.client.clone().filter(|c| !c.is_closed())
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn is_connecting(&self) -> bool {
        self.status == OracleStatus::Connecting
    }

    pub fn codespace(&self, id: &str) -> Option<&Codespace> {
        self.codespaces.iter().find(|c| c.id == id)
    }

    pub fn codespace_of_agent(&self, instance: &str) -> Option<&Codespace> {
        self.codespaces.iter().find(|c| {
            c.agent
                .as_ref()
                .and_then(|a| a.instance.as_deref())
                .is_some_and(|i| i == instance)
        })
    }

    pub fn listed_codespaces<'a>(
        &'a self,
        workspace: &'a str,
    ) -> impl Iterator<Item = &'a Codespace> + 'a {
        self.codespaces
            .iter()
            .filter(move |c| c.workspace == workspace && is_listed(c))
    }

    pub fn session_count(&self, codespace: &str) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.codespace == codespace && !s.session.archived)
            .count()
    }

    pub fn agents(&self) -> Vec<&Codespace> {
        self.codespaces
            .iter()
            .filter(|c| !matches!(c.state, CodespaceState::Closing | CodespaceState::Closed))
            .filter(|c| c.agent.as_ref().is_some_and(|a| a.instance.is_some()))
            .collect()
    }

    pub fn connect(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.status = OracleStatus::Connecting;
        self.client = None;
        self._pump = None;
        let env = self.env.clone();
        let entry = self.entry.clone();
        let task = self.backend.run(async move {
            let (client, events) = connection::connect_oracle(&env, &entry).await?;
            let lists = load(&client).await?;
            Ok::<_, String>((client, events, lists))
        });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Some(Ok((client, events, lists))) => {
                        this.retry = RETRY_INITIAL;
                        this.client = Some(client);
                        this.status = OracleStatus::Connected;
                        this.set_lists(lists);
                        let mut rx = this.backend.forward_oracle(events);
                        this._pump = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
                            while let Some(event) = next(&mut rx).await {
                                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
                                    return;
                                }
                            }
                            let _ = this.update(cx, |this, cx| {
                                if this.generation == generation {
                                    this.lost("the connection closed".into(), cx);
                                }
                            });
                        }));
                    }
                    Some(Err(err)) => this.lost(err, cx),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn lost(&mut self, reason: String, cx: &mut Context<Self>) {
        tracing::info!(oracle = self.name(), "the oracle link is down: {reason}");
        self.status = OracleStatus::Offline(reason);
        self.client = None;
        let delay = self.retry;
        self.retry = (self.retry * 2).min(RETRY_MAX);
        let generation = self.generation;
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation && this.status != OracleStatus::Connected {
                    this.connect(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client() else {
            self.connect(cx);
            return;
        };
        let generation = self.generation;
        let task = self.backend.run(async move { load(&client).await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Some(Ok(lists)) => this.set_lists(lists),
                    Some(Err(err)) => {
                        tracing::warn!("reading the lists of the oracle failed: {err}")
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_lists(&mut self, lists: Lists) {
        self.workspaces = lists.workspaces;
        self.codespaces = lists.codespaces;
        self.presets = lists.presets;
        self.hosts = lists.hosts;
        self.sessions = lists.sessions;
        self.sort();
    }

    fn sort(&mut self) {
        self.workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        self.codespaces
            .sort_by_key(|c| std::cmp::Reverse(c.created_at));
        self.presets.sort_by(|a, b| a.name.cmp(&b.name));
    }

    pub fn upsert_codespace(&mut self, codespace: Codespace, cx: &mut Context<Self>) {
        let before = self
            .codespace(&codespace.id)
            .map(|c| c.state)
            .unwrap_or(CodespaceState::Unknown);
        if before != codespace.state {
            match codespace.state {
                CodespaceState::Ready => cx.emit(OracleEvent::Ready(codespace.clone())),
                CodespaceState::Failed => cx.emit(OracleEvent::Failed(codespace.clone())),
                _ => {}
            }
        }
        match self.codespaces.iter_mut().find(|c| c.id == codespace.id) {
            Some(known) => *known = codespace,
            None => self.codespaces.push(codespace),
        }
        self.sort();
        cx.notify();
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::WorkspaceChanged { workspace } => {
                match self
                    .workspaces
                    .iter_mut()
                    .find(|w| w.name == workspace.name)
                {
                    Some(known) => *known = workspace,
                    None => self.workspaces.push(workspace),
                }
                self.sort();
            }
            Event::WorkspaceRemoved { name } => self.workspaces.retain(|w| w.name != name),
            Event::CodespaceChanged { codespace } => {
                self.upsert_codespace(codespace, cx);
            }
            Event::CodespaceProgress { id, step } => {
                if let Some(codespace) = self.codespaces.iter_mut().find(|c| c.id == id) {
                    codespace.step = Some(step.clone());
                }
                self.progress.entry(id).or_default().push(step);
            }
            Event::CodespaceRemoved { id } => {
                self.codespaces.retain(|c| c.id != id);
                self.progress.remove(&id);
                self.sessions.retain(|s| s.codespace != id);
            }
            Event::PresetChanged { preset } => {
                match self.presets.iter_mut().find(|p| p.name == preset.name) {
                    Some(known) => *known = preset,
                    None => self.presets.push(preset),
                }
                self.sort();
            }
            Event::PresetRemoved { name } => self.presets.retain(|p| p.name != name),
            Event::HostChanged { host } => {
                match self.hosts.iter_mut().find(|h| h.name == host.name) {
                    Some(known) => *known = host,
                    None => self.hosts.push(host),
                }
            }
            Event::HostRemoved { name } => self.hosts.retain(|h| h.name != name),
            Event::SessionChanged { session } => {
                match self.sessions.iter_mut().find(|s| {
                    s.codespace == session.codespace && s.session.id == session.session.id
                }) {
                    Some(known) => *known = session,
                    None => self.sessions.push(session),
                }
            }
            Event::SessionRemoved { codespace, id } => self
                .sessions
                .retain(|s| !(s.codespace == codespace && s.session.id == id)),
            Event::EventsLost => self.reload(cx),
            _ => return,
        }
        cx.notify();
    }
}

async fn load(client: &OracleClient) -> Result<Lists, String> {
    let text = |e: loom_client::ClientError| e.to_string();
    Ok(Lists {
        workspaces: client.workspaces().await.map_err(text)?,
        codespaces: client.codespaces().await.map_err(text)?,
        presets: client.presets().await.map_err(text)?,
        hosts: client.hosts().await.map_err(text)?,
        sessions: client.sessions(None).await.map_err(text)?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{AppContext, TestAppContext};
    use loom_client::oracle_proto::Role;
    use loom_client::{ClientIdentity, OracleClient, pair_oracle};
    use loom_config::{Dirs, OracleConfig};

    use super::*;

    fn dirs(root: &std::path::Path) -> Dirs {
        Dirs {
            home: root.to_path_buf(),
            config_home: root.join("config"),
            data_home: root.join("data"),
            state_home: root.join("state"),
            runtime_dir: None,
        }
    }

    fn wait(cx: &mut TestAppContext, mut done: impl FnMut(&mut gpui::App) -> bool) {
        cx.executor().allow_parking();
        for _ in 0..1000 {
            cx.run_until_parked();
            if cx.update(|cx| done(cx)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for the oracle");
    }

    #[gpui::test]
    fn a_paired_oracle_lists_and_follows_its_hosts(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let backend = Backend::new().unwrap();
        let oracle_dirs = dirs(&root.join("oracle"));
        let (oracle, shutdown) = futures::executor::block_on(backend.run(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let config = OracleConfig {
                url: Some(format!("ws://{addr}")),
                listen: addr.to_string(),
                ..Default::default()
            };
            let oracle = loom_oracle::Oracle::open(&config, &oracle_dirs).unwrap();
            let shutdown = tokio_util::sync::CancellationToken::new();
            tokio::spawn(loom_oracle::serve(
                oracle.clone(),
                listener,
                shutdown.clone(),
            ));
            for _ in 0..200 {
                if oracle.relay().online_agents() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            (oracle, shutdown)
        }))
        .unwrap();
        let env = Env {
            dirs: dirs(&root.join("client")),
            agent_bin: "/nonexistent/loom-agent".into(),
            codespaces: Default::default(),
        };
        let device = oracle
            .create_pairing(Role::Device, Duration::from_secs(60), None)
            .unwrap()
            .link;
        let pair_env = env.clone();
        let entry = futures::executor::block_on(
            backend
                .run(async move { connection::pair(&pair_env, &device, "laptop", "next").await }),
        )
        .unwrap()
        .unwrap();
        let HostEntry::Oracle { name, url, .. } = &entry else {
            panic!("{entry:?}");
        };
        assert_eq!(name, "next");
        assert!(url.starts_with("ws://127.0.0.1:"), "{url}");

        let link = cx.new(|cx| OracleLink::new(entry, env, backend.clone(), cx));
        wait(cx, |cx| link.read(cx).status == OracleStatus::Connected);
        assert!(link.read_with(cx, |link, _| link.hosts.is_empty()));

        let host = oracle
            .create_pairing(Role::Host, Duration::from_secs(60), None)
            .unwrap()
            .link;
        let _host = futures::executor::block_on(backend.run(async move {
            let identity = ClientIdentity::generate().unwrap();
            let endpoint = pair_oracle(&host, &identity, "office", None).await.unwrap();
            OracleClient::connect(&endpoint, &identity, "test")
                .await
                .unwrap()
        }))
        .unwrap();
        wait(cx, |cx| {
            link.read(cx)
                .hosts
                .iter()
                .any(|h| h.name == "office" && h.connected)
        });
        shutdown.cancel();
    }
}
