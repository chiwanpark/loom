use std::collections::HashMap;

use gpui::{AppContext, AsyncApp, Context, Entity, EventEmitter, Task, WeakEntity};
use loom_client::proto::{
    AgentInfo, ApiError, Command, Notice, SessionId, SessionInfo, TerminalId,
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

pub struct AgentConnection {
    pub host: HostEntry,
    pub info: AgentInfo,
    pub client: Client,
    pub backend: Backend,
    pub status: LinkStatus,
    pub sessions: Vec<SessionInfo>,
    pub show_archived: bool,
    conversations: HashMap<SessionId, Entity<Conversation>>,
    terminals: HashMap<TerminalId, WeakEntity<TerminalView>>,
    _pump: Option<Task<()>>,
}

impl EventEmitter<ConnectionEvent> for AgentConnection {}

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
        self.client = client;
        self.info = self.client.agent().clone();
        self.status = LinkStatus::Connected;
        self.attach(notifications, cx);
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
                if let Some(view) = self.terminals.get(&terminal).and_then(|v| v.upgrade()) {
                    view.update(cx, |view, cx| view.feed(&data, cx));
                }
            }
            Notification::TerminalExited { terminal, status } => {
                if let Some(view) = self.terminals.remove(&terminal).and_then(|v| v.upgrade()) {
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
                cx.notify();
            });
        }
        match self.sessions.iter_mut().find(|s| s.id == info.id) {
            Some(existing) => {
                existing.model = info.model;
                existing.reasoning = info.reasoning;
                existing.policy_default = info.policy_default;
                existing.archived = info.archived;
                if existing.title.is_none() {
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

    pub fn register_terminal(&mut self, id: TerminalId, view: &Entity<TerminalView>) {
        self.terminals.insert(id, view.downgrade());
    }
}

enum TerminalRequest {
    Write(Vec<u8>),
    Resize(u16, u16),
}

pub struct RemoteTerminal {
    tx: mpsc::UnboundedSender<TerminalRequest>,
}

impl RemoteTerminal {
    pub fn new(backend: &Backend, client: Client, id: TerminalId) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<TerminalRequest>();
        drop(backend.run(async move {
            while let Some(request) = rx.recv().await {
                let command = match request {
                    TerminalRequest::Write(data) => Command::TerminalInput { terminal: id, data },
                    TerminalRequest::Resize(cols, rows) => Command::ResizeTerminal {
                        terminal: id,
                        cols,
                        rows,
                    },
                };
                match client.submit(command).await {
                    Ok(reply) => {
                        tokio::spawn(async move {
                            if let Err(err) = reply.await {
                                tracing::debug!("terminal request failed: {err}");
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
            let _ = client.close_terminal(id).await;
        }));
        Self { tx }
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
