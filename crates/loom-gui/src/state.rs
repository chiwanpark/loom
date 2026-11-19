use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AppContext, AsyncApp, Context, Entity, EntityId, EventEmitter, Task, WeakEntity};
use loom_client::proto::{
    AgentInfo, ApiError, Command, DirectoryListing, ErrorCode, FileContent, FileDiff, FileVersion,
    ListeningPort, MergeStrategy, NotesContent, Notice, PolicyAction, ReasoningEffort, SessionId,
    SessionInfo, SessionState, TaskChange, TasksContent, TelemetryStatus, TerminalHolder,
    TerminalId, TerminalInfo, TerminalOwner, UsageLimits, WorkspaceChanges, WorktreeOptions,
};
use loom_client::{Client, Notification, Notifications, PortForward, terminal_owner};
use loom_config::HostEntry;
use tokio::sync::mpsc;

use crate::backend::{Backend, next};
use crate::conversation::{BULK_PAGE_ITEMS, Conversation, PAGE_ITEMS};
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
    /// The agent's notes changed; `version` is the hash of their new text.
    NotesChanged {
        version: FileVersion,
    },
    /// A session's task list changed on the agent.
    TasksChanged {
        session: SessionId,
        tasks: Vec<loom_client::proto::Task>,
    },
    /// The session list arrived (after connecting or reconnecting).
    SessionsLoaded,
    /// The agent's terminals changed: one opened, closed, or moved. The
    /// views of those that closed on another client are in `closed`.
    TerminalsChanged {
        closed: Vec<EntityId>,
    },
    /// The Files and Changes panels show another root: the worktree of
    /// another session, or the workspace.
    FileScopeChanged,
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

/// How often the agent's telemetry status is fetched; exports run every few
/// seconds (logs) to a minute (metrics).
const TELEMETRY_REFRESH: Duration = Duration::from_secs(15);

/// The last answer to `GetTelemetryStatus`.
#[derive(Default)]
pub struct TelemetryState {
    pub status: Option<TelemetryStatus>,
    /// Why the last fetch failed, such as an agent too old to answer.
    pub error: Option<String>,
    fetched: Option<Instant>,
    loading: bool,
    /// Bumped when the connection moves to another agent, so an answer from
    /// the previous one is dropped.
    generation: u64,
}

impl TelemetryState {
    fn due(&self, force: bool, now: Instant) -> bool {
        !self.loading
            && (force
                || self
                    .fetched
                    .is_none_or(|at| now.duration_since(at) >= TELEMETRY_REFRESH))
    }

    /// Forgets the answer of the previous agent.
    fn reset(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            ..Self::default()
        };
    }
}

pub struct AgentConnection {
    pub git: GitState,
    pub usage: HashMap<String, UsageState>,
    pub telemetry: TelemetryState,
    pub host: HostEntry,
    pub info: AgentInfo,
    pub client: Client,
    pub backend: Backend,
    pub status: LinkStatus,
    pub sessions: Vec<SessionInfo>,
    conversations: HashMap<SessionId, Entity<Conversation>>,
    /// Sessions whose older events are being fetched.
    loading_history: HashSet<SessionId>,
    /// Sessions to fetch every older event of, as the Changes panel needs.
    loading_all_history: HashSet<SessionId>,
    /// Completed sessions that the agent is being told were seen.
    marking_seen: HashSet<SessionId>,
    /// The session whose worktree the Files and Changes panels show; `None`
    /// for the workspace.
    file_scope: Option<SessionId>,
    /// The terminals that have a view here, held here or not.
    terminals: HashMap<TerminalId, TerminalEntry>,
    /// The agent's terminals, as it last listed them.
    listed: Vec<TerminalInfo>,
    /// This client's token for its terminals, kept across reconnects.
    pub terminal_owner: TerminalOwner,
    /// The client that forwarded ports use, the latest after a reconnect.
    live_client: Arc<std::sync::Mutex<Client>>,
    /// Local ports forwarded to ports on the agent's host, the oldest first.
    forwards: Vec<Forward>,
    _pump: Option<Task<()>>,
}

/// A local port that forwards to a port on the agent's host.
pub struct Forward {
    /// The host and port the agent connects to.
    pub host: String,
    pub port: u16,
    pub forward: Arc<PortForward>,
}

impl Forward {
    pub fn local_port(&self) -> u16 {
        self.forward.local_addr().port()
    }
}

impl EventEmitter<ConnectionEvent> for AgentConnection {}

struct TerminalEntry {
    view: WeakEntity<TerminalView>,
    io: TerminalRebind,
    /// Set while this client attaches the terminal, so lists the agent sent
    /// before do not count.
    attaching: bool,
    /// Whether a list of the agent has shown the terminal. One that is
    /// missing from a list before that is new, not closed: the agent may
    /// have made the list before the terminal opened.
    seen: bool,
}

/// How attaching a terminal went.
pub enum Attached {
    Here,
    /// Another client holds it, and it was not taken over.
    Elsewhere,
    /// The connection closed first; the view still counts the terminal as
    /// held here, so the next connection attaches it.
    Interrupted,
    Failed(String),
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
        let live_client = Arc::new(std::sync::Mutex::new(client.clone()));
        let mut this = Self {
            git: GitState::default(),
            usage: HashMap::new(),
            telemetry: TelemetryState::default(),
            host,
            info,
            client,
            backend,
            status: LinkStatus::Connected,
            sessions: Vec::new(),
            conversations: HashMap::new(),
            loading_history: HashSet::new(),
            loading_all_history: HashSet::new(),
            marking_seen: HashSet::new(),
            file_scope: None,
            terminals: HashMap::new(),
            listed: Vec::new(),
            terminal_owner: terminal_owner(),
            live_client,
            forwards: Vec::new(),
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
        // Even the same agent may have changed while the link was down.
        self.telemetry.reset();
        self.client = client;
        *self.live_client.lock().unwrap_or_else(|e| e.into_inner()) = self.client.clone();
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

    /// The open conversation of a session, if any.
    pub fn open_conversation(&self, id: &SessionId) -> Option<&Entity<Conversation>> {
        self.conversations.get(id)
    }

    /// Tells the agent that the user saw a session, when it is completed, so
    /// that it becomes idle.
    pub fn mark_seen(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        let completed = self
            .sessions
            .iter()
            .any(|s| s.id == *id && s.state == SessionState::Completed);
        if !completed
            || self.status != LinkStatus::Connected
            || !self.marking_seen.insert(id.clone())
        {
            return;
        }
        let client = self.client.clone();
        let session = id.clone();
        let task = self
            .backend
            .run(async move { client.mark_session_seen(&session).await });
        let id = id.clone();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            if let Some(Err(err)) = result {
                tracing::warn!(session = %id, error = %err, "marking a session seen failed");
            }
            let _ = this.update(cx, |this, _| this.marking_seen.remove(&id));
        })
        .detach();
    }

    fn handle(&mut self, note: Notification, cx: &mut Context<Self>) {
        match note {
            Notification::Events {
                session,
                events,
                window,
            } => {
                if let Some(conv) = self.conversations.get(&session).cloned() {
                    conv.update(cx, |conv, cx| {
                        let mut changed = false;
                        if let Some(window) = &window {
                            conv.start_window(window);
                            changed = true;
                        }
                        for event in &events {
                            changed |= conv.apply(event);
                        }
                        if changed {
                            cx.notify();
                        }
                    });
                    if window.is_some() && self.loading_all_history.contains(&session) {
                        // A new window drops what was loaded; load it again.
                        self.load_earlier(&session, BULK_PAGE_ITEMS, cx);
                    }
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
            Notification::Notice(Notice::NotesChanged { version }) => {
                cx.emit(ConnectionEvent::NotesChanged { version })
            }
            Notification::Notice(Notice::TasksChanged { session, tasks }) => {
                cx.emit(ConnectionEvent::TasksChanged { session, tasks })
            }
            Notification::Notice(Notice::TerminalsChanged(terminals)) => {
                self.apply_terminals(terminals, cx)
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
                    cx.emit(ConnectionEvent::SessionsLoaded);
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
        let conv = cx.new(|_| crate::conversation::new(info.clone()));
        self.conversations.insert(info.id.clone(), conv.clone());
        self.subscribe(info.id.clone(), None, cx);
        conv
    }

    /// Fetches about `items` conversation items before the ones a session's
    /// conversation holds, unless it holds them all or is fetching already.
    pub fn load_earlier(&mut self, id: &SessionId, items: u32, cx: &mut Context<Self>) {
        let Some(conv) = self.conversations.get(id) else {
            return;
        };
        let conv = conv.read(cx);
        let Some(before) = conv.first_seq().filter(|_| conv.has_older) else {
            self.loading_all_history.remove(id);
            return;
        };
        if !self.loading_history.insert(id.clone()) {
            return;
        }
        let client = self.client.clone();
        let session = id.clone();
        let task = self
            .backend
            .run(async move { client.history(&session, before, items).await });
        let id = id.clone();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading_history.remove(&id);
                match result {
                    Some(Ok(history)) => {
                        if let Some(conv) = this.conversations.get(&id) {
                            conv.update(cx, |conv, cx| {
                                if conv.prepend(history) {
                                    cx.notify();
                                }
                            });
                        }
                    }
                    Some(Err(err)) => {
                        tracing::warn!(session = %id, error = %err, "loading earlier events failed");
                        this.loading_all_history.remove(&id);
                    }
                    None => {}
                }
                if this.loading_all_history.contains(&id) {
                    this.load_earlier(&id, BULK_PAGE_ITEMS, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches every older event of a session's conversation, page by page.
    pub fn load_all_history(&mut self, id: &SessionId, cx: &mut Context<Self>) {
        let has_older = self
            .conversations
            .get(id)
            .is_some_and(|conv| conv.read(cx).has_older);
        if has_older && self.loading_all_history.insert(id.clone()) {
            self.load_earlier(id, BULK_PAGE_ITEMS, cx);
        }
    }

    /// Whether older events of a session are being fetched.
    pub fn is_loading_history(&self, id: &SessionId) -> bool {
        self.loading_history.contains(id)
    }

    pub fn release_conversation(&mut self, id: &SessionId) {
        self.loading_all_history.remove(id);
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
            .run(async move { client.subscribe(&session, since, Some(PAGE_ITEMS)).await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            if let Some(Ok(info)) = task.await {
                let _ = this.update(cx, |this, cx| {
                    if let Some(conv) = this.conversations.get(&id) {
                        conv.update(cx, |conv, cx| {
                            conv.info.model = info.model.clone();
                            conv.info.reasoning = info.reasoning;
                            conv.info.policy_default = info.policy_default;
                            conv.info.comment_guard = info.comment_guard;
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
                conv.info.comment_guard = info.comment_guard;
                conv.info.archived = info.archived;
                conv.info.worktree = info.worktree.clone();
                if info.title.is_some() {
                    conv.info.title = info.title.clone();
                }
                cx.notify();
            });
        }
        match self.sessions.iter_mut().find(|s| s.id == info.id) {
            Some(existing) => {
                // An open conversation follows the session's events, which
                // arrive separately from notices and are authoritative.
                if !self.conversations.contains_key(&info.id) {
                    existing.state = info.state;
                }
                existing.model = info.model;
                existing.reasoning = info.reasoning;
                existing.policy_default = info.policy_default;
                existing.comment_guard = info.comment_guard;
                existing.archived = info.archived;
                existing.worktree = info.worktree;
                if info.title.is_some() {
                    existing.title = info.title;
                }
            }
            None => self.sessions.insert(0, info),
        }
        cx.notify();
    }

    /// Deletes a session; one in a worktree loses its worktree and branch,
    /// also the changes that were not merged.
    pub fn delete_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let session = id.clone();
        let worktree = self
            .sessions
            .iter()
            .any(|s| s.id == id && s.worktree.is_some());
        let task = self.backend.run(async move {
            if worktree {
                client.remove_worktree(&session, true).await
            } else {
                client.delete_session(&session).await
            }
        });
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
        effort: ReasoningEffort,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.set_reasoning(&id, effort).await });
        self.apply_info(task, cx);
    }

    pub fn set_policy(&mut self, id: SessionId, default: PolicyAction, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.set_policy(&id, default).await });
        self.apply_info(task, cx);
    }

    pub fn set_comment_guard(&mut self, id: SessionId, enabled: bool, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.set_comment_guard(&id, enabled).await });
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
        let client = self.files_client();
        let scope = self.file_scope.clone();
        let task = self
            .backend
            .run(async move { client.workspace_changes().await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.git.loading = false;
                // The changes of a session that is no longer in view.
                if this.file_scope != scope {
                    return;
                }
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

    /// Fetches the agent's telemetry status when it is stale, or now with
    /// `force`.
    pub fn refresh_telemetry(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.status != LinkStatus::Connected || !self.telemetry.due(force, Instant::now()) {
            return;
        }
        self.telemetry.loading = true;
        let generation = self.telemetry.generation;
        let client = self.client.clone();
        let task = self
            .backend
            .run(async move { client.telemetry_status().await });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.telemetry;
                if state.generation != generation {
                    return;
                }
                state.loading = false;
                state.fetched = Some(Instant::now());
                match result {
                    Some(Ok(status)) => {
                        state.status = Some(status);
                        state.error = None;
                    }
                    Some(Err(err)) => {
                        state.status = None;
                        state.error = Some(err.to_string());
                    }
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
        committed: bool,
    ) -> impl Future<Output = Option<loom_client::Result<FileDiff>>> + use<> {
        let client = self.files_client();
        self.backend.run(async move {
            client
                .file_diff(&path, old_path.as_deref(), committed)
                .await
        })
    }

    pub fn list_directory(
        &self,
        path: String,
    ) -> impl Future<Output = Option<loom_client::Result<DirectoryListing>>> + use<> {
        let client = self.files_client();
        self.backend
            .run(async move { client.list_directory(&path).await })
    }

    pub fn read_file(
        &self,
        path: String,
    ) -> impl Future<Output = Option<loom_client::Result<FileContent>>> + use<> {
        let client = self.files_client();
        self.backend
            .run(async move { client.read_file(&path).await })
    }

    /// The client whose file, change, upload, download, and terminal
    /// commands act in the root of the session in view: its worktree, or the
    /// workspace.
    pub fn files_client(&self) -> Client {
        self.client.in_session(self.file_scope.clone())
    }

    /// The client whose file and terminal commands act in the root of
    /// `session`: its worktree, or the workspace.
    pub fn client_for(&self, session: &SessionId) -> Client {
        let worktree = self
            .sessions
            .iter()
            .any(|s| s.id == *session && s.worktree.is_some());
        self.client.in_session(worktree.then(|| session.clone()))
    }

    /// The session whose worktree the Files and Changes panels show; `None`
    /// for the workspace.
    pub fn file_scope(&self) -> Option<&SessionId> {
        self.file_scope.as_ref()
    }

    /// Makes the Files and Changes panels show the worktree of `session`, or
    /// the workspace with `None`.
    pub fn set_file_scope(&mut self, session: Option<SessionId>, cx: &mut Context<Self>) {
        if self.file_scope == session {
            return;
        }
        self.file_scope = session;
        self.git = GitState::default();
        cx.emit(ConnectionEvent::FileScopeChanged);
        cx.notify();
        self.refresh_changes(cx);
    }

    pub fn create_worktree_session(
        &self,
        options: WorktreeOptions,
    ) -> impl Future<Output = Option<loom_client::Result<SessionInfo>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.create_worktree_session(None, options).await })
    }

    /// Merges the branch of a session's worktree into its base, and
    /// returns the new commit.
    pub fn merge_worktree(
        &self,
        session: SessionId,
        strategy: MergeStrategy,
    ) -> impl Future<Output = Option<loom_client::Result<String>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.merge_worktree(&session, strategy, false).await })
    }

    /// Pushes the branch of a session's worktree and opens a pull request,
    /// and returns its URL.
    pub fn open_pull_request(
        &self,
        session: SessionId,
        title: String,
        body: String,
        draft: bool,
    ) -> impl Future<Output = Option<loom_client::Result<String>>> + use<> {
        let client = self.client.clone();
        self.backend.run(async move {
            client
                .open_pull_request(&session, &title, &body, draft)
                .await
        })
    }

    /// Where the branch of a session's worktree is against its base.
    pub fn worktree_status(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Option<loom_client::Result<loom_client::proto::WorktreeStatus>>> + use<>
    {
        let client = self.client.clone();
        self.backend
            .run(async move { client.worktree_status(&session).await })
    }

    /// The TCP ports that programs listen on, on the agent's host.
    pub fn list_ports(
        &self,
    ) -> impl Future<Output = Option<loom_client::Result<Vec<ListeningPort>>>> + use<> {
        let client = self.client.clone();
        self.backend.run(async move { client.list_ports().await })
    }

    pub fn forwards(&self) -> &[Forward] {
        &self.forwards
    }

    /// Forwards a local port to `host:port` on the agent's host: `port`
    /// itself when it is free here, else any free port. The task says which
    /// local port, or why none.
    pub fn forward_port(
        &mut self,
        host: String,
        port: u16,
        cx: &mut Context<Self>,
    ) -> Task<Result<u16, String>> {
        if let Some(forward) = self
            .forwards
            .iter()
            .find(|f| f.host == host && f.port == port)
        {
            return Task::ready(Ok(forward.local_port()));
        }
        let live = self.live_client.clone();
        let started = self.backend.run({
            let host = host.clone();
            async move {
                PortForward::start(None, host, port, move || {
                    let client = live.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    (!client.is_closed()).then_some(client)
                })
                .await
            }
        });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let forward = match started.await {
                Some(Ok(forward)) => Arc::new(forward),
                Some(Err(err)) => return Err(format!("could not listen on a local port: {err}")),
                None => return Err("the client is shutting down".into()),
            };
            let local = forward.local_addr().port();
            this.update(cx, |this, cx| {
                this.forwards.push(Forward {
                    host,
                    port,
                    forward,
                });
                cx.notify();
            })
            .map_err(|_| "the agent's connection is gone".to_string())?;
            Ok(local)
        })
    }

    /// Stops forwarding the local port `local`; its connections close.
    pub fn stop_forward(&mut self, local: u16, cx: &mut Context<Self>) {
        self.forwards.retain(|f| f.local_port() != local);
        cx.notify();
    }

    pub fn read_file_bytes(
        &self,
        path: String,
        max: u64,
    ) -> impl Future<Output = Option<loom_client::Result<Vec<u8>>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.read_file_bytes(&path, max).await })
    }

    pub fn write_file(
        &self,
        path: String,
        text: String,
        expected: FileVersion,
    ) -> impl Future<Output = Option<loom_client::Result<FileVersion>>> + use<> {
        let client = self.files_client();
        self.backend
            .run(async move { client.write_file(&path, text, expected).await })
    }

    pub fn read_notes(
        &self,
    ) -> impl Future<Output = Option<loom_client::Result<NotesContent>>> + use<> {
        let client = self.client.clone();
        self.backend.run(async move { client.notes().await })
    }

    pub fn read_tasks(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Option<loom_client::Result<TasksContent>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.tasks(&session).await })
    }

    pub fn update_tasks(
        &self,
        session: SessionId,
        changes: Vec<TaskChange>,
    ) -> impl Future<Output = Option<loom_client::Result<TasksContent>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.update_tasks(&session, changes).await })
    }

    /// The system prompt and the tools of the session's next model call.
    pub fn read_system_prompt(
        &self,
        session: SessionId,
    ) -> impl Future<Output = Option<loom_client::Result<loom_client::proto::SystemPrompt>>> + use<>
    {
        let client = self.client.clone();
        self.backend
            .run(async move { client.system_prompt(&session).await })
    }

    pub fn write_notes(
        &self,
        text: String,
        expected: FileVersion,
    ) -> impl Future<Output = Option<loom_client::Result<FileVersion>>> + use<> {
        let client = self.client.clone();
        self.backend
            .run(async move { client.write_notes(text, expected).await })
    }

    /// Routes a terminal's output to its view. `attaching` marks a terminal
    /// the caller attaches next, with [`Self::attach_terminal`].
    pub fn register_terminal(
        &mut self,
        id: TerminalId,
        view: &Entity<TerminalView>,
        io: TerminalRebind,
        attaching: bool,
    ) {
        self.terminals.insert(
            id,
            TerminalEntry {
                view: view.downgrade(),
                io,
                attaching,
                seen: false,
            },
        );
    }

    /// Stops routing a terminal to its view without closing it on the agent
    /// (a restored terminal that could not be reattached).
    pub fn unregister_terminal(&mut self, id: TerminalId) {
        if let Some(terminal) = self.terminals.remove(&id) {
            terminal.io.forget();
        }
    }

    /// The agent's ID for a terminal view opened through this connection.
    pub fn terminal_id(&self, view: &Entity<TerminalView>) -> Option<TerminalId> {
        self.terminals
            .iter()
            .find(|(_, t)| t.view.entity_id() == view.entity_id())
            .map(|(id, _)| *id)
    }

    /// Drops the terminals whose views are gone.
    fn prune_terminals(&mut self) {
        self.terminals.retain(|_, t| t.view.upgrade().is_some());
    }

    /// Takes in the agent's list of terminals: tells each view where its
    /// terminal is attached, and forgets the views of terminals that closed
    /// on another client. The agent sends only its latest list, so the list
    /// that shows another client with the terminal may never come: a
    /// terminal held here that a list showed before and that is now gone
    /// closed elsewhere too.
    fn apply_terminals(&mut self, terminals: Vec<TerminalInfo>, cx: &mut Context<Self>) {
        self.prune_terminals();
        let mut closed = Vec::new();
        for (id, entry) in &mut self.terminals {
            let Some(view) = entry.view.upgrade() else {
                continue;
            };
            if entry.attaching {
                continue;
            }
            let held = *view.read(cx).holder() == TerminalHolder::You;
            let info = terminals.iter().find(|t| t.id == *id);
            if info.is_some() {
                entry.seen = true;
            }
            match info {
                // Held by no one: this client's earlier connection, until
                // the new one attaches it again.
                Some(info) if held && info.holder == TerminalHolder::Nobody => {}
                Some(info) => view.update(cx, |view, cx| view.set_holder(info.holder.clone(), cx)),
                // One held here and not listed yet is new. One that exited
                // is gone already: the agent says so before it lists the rest.
                None if held && !entry.seen => {}
                None => closed.push(*id),
            }
        }
        let closed = closed
            .into_iter()
            .filter_map(|id| self.terminals.remove(&id))
            .map(|entry| {
                entry.io.forget();
                entry.view.entity_id()
            })
            .collect();
        self.listed = terminals;
        cx.emit(ConnectionEvent::TerminalsChanged { closed });
        cx.notify();
    }

    /// The terminals that another client holds, or none does, which have
    /// no view here.
    pub fn untracked_terminals(&mut self) -> Vec<TerminalInfo> {
        self.prune_terminals();
        self.listed
            .iter()
            .filter(|t| t.holder != TerminalHolder::You)
            .filter(|t| !self.terminals.contains_key(&t.id))
            .cloned()
            .collect()
    }

    /// Attaches a terminal with a view here: its view starts over with the
    /// terminal's recent output. Without `take_over`, a terminal another
    /// client holds stays there.
    pub fn attach_terminal(
        &mut self,
        id: TerminalId,
        take_over: bool,
        cx: &mut Context<Self>,
    ) -> Task<Attached> {
        let Some(entry) = self.terminals.get_mut(&id) else {
            return Task::ready(Attached::Failed("the terminal is not open here".into()));
        };
        let Some(view) = entry.view.upgrade() else {
            return Task::ready(Attached::Failed("the terminal's tab is closed".into()));
        };
        entry.attaching = true;
        entry.io.rebind(self.client.clone());
        // Cleared before asking, so the replay that comes first lands on an
        // empty screen.
        view.update(cx, |view, cx| view.restart(cx));
        let (cols, rows) = view.read(cx).grid_size();
        let client = self.client.clone();
        let owner = self.terminal_owner;
        let task = self.backend.run(async move {
            client
                .attach_terminal(id, cols, rows, owner, take_over)
                .await
        });
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            this.update(cx, |this, cx| {
                // The agent's list, which tells who holds it when this did not.
                let holder = match this
                    .listed
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.holder.clone())
                {
                    Some(TerminalHolder::You) | None => TerminalHolder::Nobody,
                    Some(other) => other,
                };
                if let Some(entry) = this.terminals.get_mut(&id) {
                    entry.attaching = false;
                }
                match result {
                    Some(Ok(())) => Attached::Here,
                    Some(Err(err)) if err.api().map(|e| e.code) == Some(ErrorCode::Conflict) => {
                        view.update(cx, |view, cx| view.set_holder(holder, cx));
                        Attached::Elsewhere
                    }
                    Some(Err(err)) => Attached::Failed(err.to_string()),
                    // The next connection attaches it again.
                    None => Attached::Interrupted,
                }
            })
            .unwrap_or(Attached::Interrupted)
        })
    }

    /// Moves a terminal another client holds, or none does, here.
    pub fn take_terminal(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let Some(id) = self.terminal_id(view) else {
            return;
        };
        let task = self.attach_terminal(id, true, cx);
        let view = view.downgrade();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            if let Attached::Failed(failed) = task.await {
                tracing::debug!(terminal = id, "cannot take the terminal over: {failed}");
                let _ = this.update(cx, |this, cx| {
                    this.unregister_terminal(id);
                    if let Some(view) = view.upgrade() {
                        view.update(cx, |view, cx| {
                            view.set_lost(&format!("cannot take the terminal over: {failed}"), cx)
                        });
                    }
                });
            }
        })
        .detach();
    }

    /// After a reconnect, points every terminal at the new client and
    /// attaches the ones that were held here again; another client may
    /// have taken them meanwhile. Terminals of a restarted agent (a new
    /// instance) are gone, so they are ended instead; the new agent may
    /// reuse their IDs.
    fn reattach_terminals(&mut self, same_agent: bool, cx: &mut Context<Self>) {
        self.prune_terminals();
        if !same_agent {
            self.listed.clear();
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
        let mut held = Vec::new();
        for (&id, terminal) in &self.terminals {
            terminal.io.rebind(self.client.clone());
            let here = terminal
                .view
                .upgrade()
                .is_some_and(|view| *view.read(cx).holder() == TerminalHolder::You);
            if here || terminal.attaching {
                held.push(id);
            }
        }
        for id in held {
            let task = self.attach_terminal(id, false, cx);
            cx.spawn(async move |this, cx: &mut AsyncApp| {
                let Attached::Failed(failed) = task.await else {
                    return;
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
