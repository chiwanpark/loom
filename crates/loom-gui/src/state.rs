use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{AppContext, AsyncApp, Context, Entity, EventEmitter, Task, WeakEntity};
use loom_client::proto::{
    AgentInfo, ApiError, Command, DirectoryListing, FileContent, FileDiff, Notice, PolicyAction,
    ReasoningEffort, SessionId, SessionInfo, TerminalId, UsageLimits, WorkspaceChanges,
};
use loom_client::{Client, Notification, Notifications};
use loom_config::HostEntry;
use tokio::sync::mpsc;

use crate::backend::{Backend, next};
use crate::conversation::Conversation;
use crate::term::{TerminalIo, TerminalView};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkStatus {
    Connected,
    Disconnected(String),
}

pub enum ConnectionEvent {
    Notice(Notice),
    LoginFinished(Result<String, ApiError>),
    Failed(String),
    SessionRemoved(SessionId),
    Closed,
}

#[derive(Default)]
pub struct GitState {
    pub changes: Option<Rc<WorkspaceChanges>>,
    pub error: Option<String>,
    pub revision: u64,
    loading: bool,
}

/// How often a provider's subscription limits are fetched while nothing
/// else prompts a refresh.
const USAGE_REFRESH: Duration = Duration::from_secs(120);

/// The last answer to `GetUsageLimits` for one provider.
#[derive(Default)]
pub struct UsageState {
    /// `None` until fetched, and when the provider cannot report limits.
    pub limits: Option<Rc<UsageLimits>>,
    /// Why the last fetch failed; older `limits` are kept meanwhile.
    pub error: Option<String>,
    fetched: Option<Instant>,
    /// The caller's marker at the last fetch, so a change (such as a finished
    /// turn) refreshes before the interval runs out.
    marker: u64,
    loading: bool,
}

impl UsageState {
    fn due(&self, marker: Option<u64>, now: Instant) -> bool {
        !self.loading
            && match self.fetched {
                None => true,
                Some(at) => {
                    marker.is_some_and(|m| m != self.marker)
                        || now.duration_since(at) >= USAGE_REFRESH
                }
            }
    }
}

pub struct AgentConnection {
    pub git: GitState,
    pub usage: HashMap<String, UsageState>,
    pub host: HostEntry,
    pub info: AgentInfo,
    pub client: Client,
    pub backend: Backend,
    pub status: LinkStatus,
    pub sessions: Vec<SessionInfo>,
    pub show_archived: bool,
    conversations: HashMap<SessionId, Entity<Conversation>>,
    terminals: HashMap<TerminalId, TerminalEntry>,
    _pump: Option<Task<()>>,
}

impl EventEmitter<ConnectionEvent> for AgentConnection {}

struct TerminalEntry {
    view: WeakEntity<TerminalView>,
    io: TerminalRebind,
}

impl AgentConnection {
    pub fn new(
        host: HostEntry,
        client: Client,
        notifications: Notifications,
        backend: Backend,
        cx: &mut Context<Self>,
    ) -> Self {
        let info = client.agent().clone();
        let mut this = Self {
            git: GitState::default(),
            usage: HashMap::new(),
            host,
            info,
            client,
            backend,
            status: LinkStatus::Connected,
            sessions: Vec::new(),
            show_archived: false,
            conversations: HashMap::new(),
            terminals: HashMap::new(),
            _pump: None,
        };
        this.attach(notifications, cx);
        this.refresh_sessions(cx);
        this
    }

    pub fn key(&self) -> (String, String) {
        (self.host.name().to_owned(), self.info.instance.0.clone())
    }

    pub fn title(&self) -> String {
        let workspace = self.info.workspace.display().to_string();
        format!("{} · {}", self.host.name(), workspace)
    }

    fn attach(&mut self, notifications: Notifications, cx: &mut Context<Self>) {
        let mut rx = self.backend.forward(notifications);
        self._pump = Some(cx.spawn(async move |this, cx: &mut AsyncApp| {
            while let Some(note) = next(&mut rx).await {
                if this.update(cx, |this, cx| this.handle(note, cx)).is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.status = LinkStatus::Disconnected("the connection closed".into());
                cx.emit(ConnectionEvent::Closed);
                cx.notify();
            });
        }));
    }

    pub fn reattach(
        &mut self,
        client: Client,
        notifications: Notifications,
        cx: &mut Context<Self>,
    ) {
        let same_agent = client.agent().instance == self.info.instance;
        self.client = client;
        self.info = self.client.agent().clone();
        self.status = LinkStatus::Connected;
        self.attach(notifications, cx);
        self.reattach_terminals(same_agent, cx);
        self.refresh_sessions(cx);
        let subscriptions: Vec<(SessionId, u64)> = self
            .conversations
            .iter()
            .map(|(id, conv)| (id.clone(), conv.read(cx).last_seq))
            .collect();
        for (id, since) in subscriptions {
            self.subscribe(id, Some(since), cx);
        }
        cx.notify();
    }

    fn handle(&mut self, note: Notification, cx: &mut Context<Self>) {
        match note {
            Notification::Event { session, event } => {
                if let Some(conv) = self.conversations.get(&session) {
                    conv.update(cx, |conv, cx| {
                        if conv.apply(&event) {
                            cx.notify();
                        }
                    });
                    let state = conv.read(cx).state;
                    let title = conv.read(cx).info.title.clone();
                    if let Some(info) = self.sessions.iter_mut().find(|s| s.id == session) {
                        info.state = state;
                        if info.title.is_none() {
                            info.title = title;
                        }
                    }
                    cx.notify();
                }
            }
            Notification::LoginFinished { result, .. } => {
                let result = result.map(|account| {
                    account
                        .label
                        .or(account.account_id)
                        .unwrap_or_else(|| account.provider.to_string())
                });
                cx.emit(ConnectionEvent::LoginFinished(result));
            }
            Notification::Notice(Notice::SessionUpdated(info)) => {
                self.update_session_info(info, cx)
            }
            Notification::Notice(Notice::SessionRemoved(id)) => {
                self.sessions.retain(|s| s.id != id);
                self.conversations.remove(&id);
                cx.emit(ConnectionEvent::SessionRemoved(id));
                cx.notify();
            }
            Notification::Notice(notice) => cx.emit(ConnectionEvent::Notice(notice)),
            Notification::TerminalOutput { terminal, data } => {
                if let Some(view) = self.terminals.get(&terminal).and_then(|t| t.view.upgrade()) {
                    view.update(cx, |view, cx| view.feed(&data, cx));
                }
            }
            Notification::TerminalExited { terminal, status } => {
                if let Some(view) = self
                    .terminals
                    .remove(&terminal)
                    .and_then(|t| t.view.upgrade())
                {
                    view.update(cx, |view, cx| view.set_exited(status, cx));
                }
            }
        }
    }

    pub fn refresh_sessions(&mut self, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.list_sessions().await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            if let Some(Ok(mut sessions)) = task.await {
                sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
                let _ = this.update(cx, |this, cx| {
                    this.sessions = sessions;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn conversation(
        &mut self,
        info: &SessionInfo,
        cx: &mut Context<Self>,
    ) -> Entity<Conversation> {
        if let Some(conv) = self.conversations.get(&info.id) {
            return conv.clone();
        }
        let conv = cx.new(|_| Conversation::new(info.clone()));
        self.conversations.insert(info.id.clone(), conv.clone());
        self.subscribe(info.id.clone(), None, cx);
        conv
    }

    pub fn release_conversation(&mut self, id: &SessionId) {
        if self.conversations.remove(id).is_some() {
            let client = self.client.clone();
            let id = id.clone();
            drop(
                self.backend
                    .run(async move { client.unsubscribe(&id).await }),
            );
        }
    }

    fn subscribe(&mut self, id: SessionId, since: Option<u64>, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let session = id.clone();
        let task = self
            .backend
            .run(async move { client.subscribe(&session, since).await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            if let Some(Ok(info)) = task.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(conv) = this.conversations.get(&id) {
                        conv.update(cx, |conv, cx| {
                            conv.info.model = info.model.clone();
                            conv.info.reasoning = info.reasoning;
                            conv.info.policy_default = info.policy_default;
                            conv.state = info.state;
                            cx.notify();
                        });
                    }
                });
            }
        })
        .detach();
    }

    pub fn update_session_info(&mut self, info: SessionInfo, cx: &mut Context<Self>) {
        if let Some(conv) = self.conversations.get(&info.id) {
            conv.update(cx, |conv, cx| {
                conv.info.model = info.model.clone();
                conv.info.reasoning = info.reasoning;
                conv.info.policy_default = info.policy_default;
                conv.info.archived = info.archived;
                if info.title.is_some() {
                    conv.info.title = info.title.clone();
                }
                cx.notify();
            });
        }
        match self.sessions.iter_mut().find(|s| s.id == info.id) {
            Some(existing) => {
                existing.model = info.model;
                existing.reasoning = info.reasoning;
                existing.policy_default = info.policy_default;
                existing.archived = info.archived;
                if info.title.is_some() {
                    existing.title = info.title;
                }
            }
            None => self.sessions.insert(0, info),
        }
        cx.notify();
    }

    pub fn delete_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let session = id.clone();
        let task = self
            .backend
            .run(async move { client.delete_session(&session).await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Some(Ok(())) => {
                    this.sessions.retain(|s| s.id != id);
                    this.conversations.remove(&id);
                    cx.emit(ConnectionEvent::SessionRemoved(id));
                    cx.notify();
                }
                Some(Err(err)) => cx.emit(ConnectionEvent::Failed(err.to_string())),
                None => {}
            });
        })
        .detach();
    }

    pub fn rename(&mut self, id: SessionId, title: Option<String>, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.rename_session(&id, title).await });
        self.apply_info(task, cx);
    }

    pub fn set_archived(&mut self, id: SessionId, archived: bool, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.archive_session(&id, archived).await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Some(Ok(info)) => this.update_session_info(info, cx),
                Some(Err(err)) => cx.emit(ConnectionEvent::Failed(err.to_string())),
                None => {}
            });
        })
        .detach();
    }

    pub fn set_reasoning(
        &mut self,
        id: SessionId,
        effort: Option<ReasoningEffort>,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.set_reasoning(&id, effort).await });
        self.apply_info(task, cx);
    }

    pub fn set_policy(
        &mut self,
        id: SessionId,
        default: Option<PolicyAction>,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.set_policy(&id, default).await });
        self.apply_info(task, cx);
    }

    fn apply_info(
        &mut self,
        task: impl Future<Output = Option<loom_client::Result<SessionInfo>>> + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Some(Ok(info)) => this.update_session_info(info, cx),
                Some(Err(err)) => cx.emit(ConnectionEvent::Failed(err.to_string())),
                None => {}
            });
        })
        .detach();
    }

    pub fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        if self.git.loading || self.status != LinkStatus::Connected {
            return;
        }
        self.git.loading = true;
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.workspace_changes().await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.git.loading = false;
                match result {
                    Some(Ok(changes)) => {
                        let error = this.git.error.take();
                        if this.git.changes.as_deref() != Some(&changes) || error.is_some() {
                            this.git.changes = Some(Rc::new(changes));
                            this.git.revision += 1;
                            cx.notify();
                        }
                    }
                    Some(Err(err)) => {
                        let message = err.to_string();
                        if this.git.error.as_ref() != Some(&message) {
                            this.git.error = Some(message);
                            this.git.revision += 1;
                            cx.notify();
                        }
                    }
                    None => {}
                }
            });
        })
        .detach();
    }

    /// Fetches `provider`'s usage limits when they are stale or `marker`
    /// changed since the last fetch. A `None` marker (a turn in progress)
    /// leaves only the interval.
    pub fn refresh_usage(&mut self, provider: &str, marker: Option<u64>, cx: &mut Context<Self>) {
        if self.status != LinkStatus::Connected {
            return;
        }
        let state = self.usage.entry(provider.to_owned()).or_default();
        if !state.due(marker, Instant::now()) {
            return;
        }
        state.loading = true;
        if let Some(marker) = marker {
            state.marker = marker;
        }
        let client = self.client.clone();
        let name = provider.to_owned();
        let task = self
            .backend
            .run(async move { client.usage_limits(&name).await });
        let provider = provider.to_owned();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(state) = this.usage.get_mut(&provider) else {
                    return;
                };
                state.loading = false;
                state.fetched = Some(Instant::now());
                match result {
                    Some(Ok(limits)) => {
                        state.limits = limits.map(Rc::new);
                        state.error = None;
                    }
                    Some(Err(err)) => state.error = Some(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn file_diff(
        &self,
        path: String,
        old_path: Option<String>,
    ) -> impl Future<Output = Option<loom_client::Result<FileDiff>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.file_diff(&path, old_path.as_deref()).await })
    }

    pub fn list_directory(
        &self,
        path: String,
    ) -> impl Future<Output = Option<loom_client::Result<DirectoryListing>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.list_directory(&path).await })
    }

    pub fn read_file(
        &self,
        path: String,
    ) -> impl Future<Output = Option<loom_client::Result<FileContent>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.read_file(&path).await })
    }

    pub fn register_terminal(
        &mut self,
        id: TerminalId,
        view: &Entity<TerminalView>,
        io: TerminalRebind,
    ) {
        self.terminals.insert(
            id,
            TerminalEntry {
                view: view.downgrade(),
                io,
            },
        );
    }

    /// After a reconnect, points every open terminal at the new client and takes it
    /// over with `AttachTerminal`. Terminals of a restarted agent (a new instance)
    /// are gone, so they are ended instead; the new agent may reuse their IDs.
    fn reattach_terminals(&mut self, same_agent: bool, cx: &mut Context<Self>) {
        self.terminals.retain(|_, t| t.view.upgrade().is_some());
        if !same_agent {
            for (_, terminal) in self.terminals.drain() {
                terminal.io.forget();
                if let Some(view) = terminal.view.upgrade() {
                    view.update(cx, |view, cx| {
                        view.set_lost("the agent restarted; this terminal is gone", cx)
                    });
                }
            }
            return;
        }
        for (&id, terminal) in &self.terminals {
            let Some(view) = terminal.view.upgrade() else {
                continue;
            };
            terminal.io.rebind(self.client.clone());
            let (cols, rows) = view.read(cx).grid_size();
            let client = self.client.clone();
            let task = self
                .backend
                .run(async move { client.attach_terminal(id, cols, rows).await });
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                let failed = match task.await {
                    Some(Ok(())) => return,
                    Some(Err(err)) => err.to_string(),
                    None => "the connection closed".into(),
                };
                tracing::debug!(terminal = id, "cannot reattach the terminal: {failed}");
                let _ = this.update(cx, |this, cx| {
                    if let Some(terminal) = this.terminals.remove(&id) {
                        terminal.io.forget();
                        if let Some(view) = terminal.view.upgrade() {
                            view.update(cx, |view, cx| {
                                view.set_lost("the terminal closed while disconnected", cx)
                            });
                        }
                    }
                });
            })
            .detach();
        }
    }
}

enum TerminalRequest {
    Write(Vec<u8>),
    Resize(u16, u16),
    /// Use this client from now on (after a reconnect), or none if the terminal is gone.
    Rebind(Option<Client>),
}

/// Lets the connection retarget a [`RemoteTerminal`] after a reconnect. It holds a
/// weak sender, so dropping the view still ends the I/O task and closes the terminal.
#[derive(Clone)]
pub struct TerminalRebind {
    tx: mpsc::WeakUnboundedSender<TerminalRequest>,
}

impl TerminalRebind {
    fn send(&self, client: Option<Client>) {
        if let Some(tx) = self.tx.upgrade() {
            let _ = tx.send(TerminalRequest::Rebind(client));
        }
    }

    fn rebind(&self, client: Client) {
        self.send(Some(client));
    }

    fn forget(&self) {
        self.send(None);
    }
}

pub struct RemoteTerminal {
    tx: mpsc::UnboundedSender<TerminalRequest>,
}

impl RemoteTerminal {
    pub fn new(backend: &Backend, client: Client, id: TerminalId) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<TerminalRequest>();
        drop(backend.run(async move {
            let mut client = Some(client);
            while let Some(request) = rx.recv().await {
                let command = match request {
                    TerminalRequest::Write(data) => Command::TerminalInput { terminal: id, data },
                    TerminalRequest::Resize(cols, rows) => Command::ResizeTerminal {
                        terminal: id,
                        cols,
                        rows,
                    },
                    TerminalRequest::Rebind(next) => {
                        client = next;
                        continue;
                    }
                };
                // While disconnected, keystrokes and resizes are dropped; the size is
                // sent again when the terminal is reattached.
                let Some(current) = client.as_ref().filter(|c| !c.is_closed()) else {
                    continue;
                };
                match current.submit(command).await {
                    Ok(reply) => {
                        tokio::spawn(async move {
                            if let Err(err) = reply.await {
                                tracing::debug!("terminal request failed: {err}");
                            }
                        });
                    }
                    Err(err) => tracing::debug!("terminal request not sent: {err}"),
                }
            }
            // The view is gone: close the terminal on the agent.
            if let Some(client) = client.filter(|c| !c.is_closed()) {
                let _ = client.close_terminal(id).await;
            }
        }));
        Self { tx }
    }

    pub fn rebinder(&self) -> TerminalRebind {
        TerminalRebind {
            tx: self.tx.downgrade(),
        }
    }
}

impl TerminalIo for RemoteTerminal {
    fn write(&self, data: Vec<u8>) {
        let _ = self.tx.send(TerminalRequest::Write(data));
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(TerminalRequest::Resize(cols, rows));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_refetched_when_stale_or_the_marker_changes() {
        let now = Instant::now();
        let mut state = UsageState::default();
        assert!(state.due(None, now), "never fetched");
        state.fetched = Some(now);
        state.marker = 7;
        assert!(!state.due(Some(7), now + Duration::from_secs(60)));
        assert!(
            !state.due(None, now + Duration::from_secs(60)),
            "turn running"
        );
        assert!(
            state.due(Some(8), now + Duration::from_secs(1)),
            "turn finished"
        );
        assert!(state.due(None, now + USAGE_REFRESH), "interval elapsed");
        state.loading = true;
        assert!(
            !state.due(Some(8), now + USAGE_REFRESH),
            "already in flight"
        );
    }
}
