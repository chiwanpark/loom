//! A connection to one agent through its host's relay. It keeps the open
//! conversations up to date from the agent's events and hands the app what
//! changed in batches, so a slow UI sees one update for many events.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use loom_client::proto::{ContentPart, InstanceId, SessionId, SessionInfo};
use loom_client::{Client, ClientError, ClientIdentity, Notification, Notifications, RelayHost};
use loom_view_model::conversation::{Conversation, PAGE_ITEMS};
use tokio::sync::Notify;

use crate::error::{LoomError, Result};
use crate::hosts::CLIENT_NAME;
use crate::records::{
    Agent, AgentUpdate, ApprovalDecision, ConversationUpdate, Delivery, ImageData, Model, Notes,
    Reasoning, SessionSummary, TakenBack, TaskEdit, TaskItem, notice_message, parse_version,
    task_items, version_text,
};
use crate::runtime::{run, spawn};

#[derive(uniffi::Object)]
pub struct AgentConnection {
    shared: Arc<Shared>,
}

struct Shared {
    relay: RelayHost,
    instance: InstanceId,
    identity: Arc<ClientIdentity>,
    state: Mutex<State>,
    /// Wakes the app's wait for updates; it keeps a permit when no one waits.
    wake: Notify,
}

struct State {
    agent: Agent,
    client: Option<Client>,
    /// Counts the links, so a lost link's events and end are ignored once
    /// there is a newer one.
    epoch: u64,
    closed: bool,
    link: Option<(bool, Option<String>)>,
    sessions: Vec<SessionInfo>,
    sessions_changed: bool,
    open: HashMap<SessionId, Open>,
    /// Updates that go out in the order they came.
    queue: Vec<AgentUpdate>,
}

struct Open {
    conversation: Conversation,
    /// The generation, prepended items and revision of the conversation the
    /// app has.
    sent: Option<(u64, usize, u64)>,
    changed: bool,
}

impl Open {
    fn new(info: SessionInfo) -> Self {
        Self {
            conversation: Conversation::new(info),
            sent: None,
            changed: true,
        }
    }

    fn update(&mut self) -> ConversationUpdate {
        let conversation = &self.conversation;
        // Older items move every index, so the app gets them all again; it
        // keeps its place by the items' keys.
        let (reset, revision) = match self.sent {
            Some((generation, prepended, revision))
                if generation == conversation.generation && prepended == conversation.prepended =>
            {
                (false, revision)
            }
            _ => (true, 0),
        };
        self.sent = Some((
            conversation.generation,
            conversation.prepended,
            conversation.revision,
        ));
        self.changed = false;
        ConversationUpdate::of(conversation, revision, reset)
    }
}

impl State {
    fn take_updates(&mut self) -> Vec<AgentUpdate> {
        let mut updates = Vec::new();
        if let Some((connected, error)) = self.link.take() {
            updates.push(AgentUpdate::Link { connected, error });
        }
        if std::mem::take(&mut self.sessions_changed) {
            updates.push(AgentUpdate::Sessions {
                sessions: self.sessions.iter().map(SessionSummary::from).collect(),
            });
        }
        for open in self.open.values_mut().filter(|open| open.changed) {
            updates.push(AgentUpdate::Conversation {
                update: open.update(),
            });
        }
        updates.append(&mut self.queue);
        updates
    }

    /// Takes in a session's new settings and state.
    fn session_updated(&mut self, info: SessionInfo) {
        if let Some(open) = self.open.get_mut(&info.id) {
            let conversation = &mut open.conversation;
            conversation.info.model = info.model.clone();
            conversation.info.reasoning = info.reasoning;
            conversation.info.policy_default = info.policy_default;
            conversation.info.archived = info.archived;
            if info.title.is_some() {
                conversation.info.title = info.title.clone();
            }
            open.changed = true;
        }
        match self.sessions.iter_mut().find(|s| s.id == info.id) {
            Some(known) => *known = info,
            None => self.sessions.push(info),
        }
        self.sessions_changed = true;
    }

    fn handle(&mut self, notification: Notification) {
        match notification {
            Notification::Events {
                session,
                events,
                window,
            } => {
                let Some(open) = self.open.get_mut(&session) else {
                    return;
                };
                let mut changed = false;
                if let Some(window) = &window {
                    open.conversation.start_window(window);
                    changed = true;
                }
                for event in &events {
                    changed |= open.conversation.apply(event);
                }
                if !changed {
                    return;
                }
                open.changed = true;
                let (state, title) = (
                    open.conversation.state,
                    open.conversation.info.title.clone(),
                );
                if let Some(info) = self.sessions.iter_mut().find(|s| s.id == session)
                    && (info.state != state || (info.title.is_none() && title.is_some()))
                {
                    info.state = state;
                    if info.title.is_none() {
                        info.title = title;
                    }
                    self.sessions_changed = true;
                }
            }
            Notification::Notice(notice) => match notice {
                loom_client::proto::Notice::SessionUpdated(info) => self.session_updated(info),
                loom_client::proto::Notice::SessionRemoved(id) => {
                    self.sessions.retain(|s| s.id != id);
                    self.open.remove(&id);
                    self.sessions_changed = true;
                }
                loom_client::proto::Notice::NotesChanged { version } => {
                    self.queue.push(AgentUpdate::NotesChanged {
                        version: version_text(&version),
                    });
                }
                loom_client::proto::Notice::TasksChanged { session, tasks } => {
                    self.queue.push(AgentUpdate::TasksChanged {
                        session: session.0,
                        tasks: task_items(&tasks),
                    });
                }
                notice => {
                    if let Some((level, message)) = notice_message(&notice) {
                        self.queue.push(AgentUpdate::Notice { level, message });
                    }
                }
            },
            Notification::LoginFinished { .. }
            | Notification::TerminalOutput { .. }
            | Notification::TerminalExited { .. } => {}
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn client(&self) -> Result<Client> {
        self.lock().client.clone().ok_or(LoomError::Closed)
    }

    async fn connect(&self) -> Result<(Client, Notifications)> {
        let stream = self
            .relay
            .connect(Some(&self.instance), &self.identity)
            .await?;
        Ok(Client::connect(stream, CLIENT_NAME).await?)
    }

    /// Starts following a new link: its events, and its end.
    fn attach(self: &Arc<Self>, client: Client, mut notifications: Notifications) {
        let epoch = {
            let mut state = self.lock();
            state.epoch += 1;
            state.agent = Agent::from(client.agent());
            state.client = Some(client);
            state.link = Some((true, None));
            state.epoch
        };
        self.wake.notify_one();
        let shared = Arc::downgrade(self);
        spawn(async move {
            while let Some(notification) = notifications.recv().await {
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                let mut state = shared.lock();
                if state.epoch != epoch {
                    return;
                }
                state.handle(notification);
                drop(state);
                shared.wake.notify_one();
            }
            if let Some(shared) = shared.upgrade() {
                let mut state = shared.lock();
                if state.epoch == epoch && !state.closed {
                    state.client = None;
                    state.link = Some((false, Some("Lost the connection to the agent.".into())));
                    drop(state);
                    shared.wake.notify_one();
                }
            }
        });
    }

    async fn refresh_sessions(&self) -> Result<()> {
        let sessions = self.client()?.list_sessions().await?;
        let mut state = self.lock();
        state.sessions = sessions;
        state.sessions_changed = true;
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    /// Subscribes to an open session, from `since` on, and takes in the
    /// settings the agent answers with.
    async fn subscribe(&self, id: &SessionId, since: Option<u64>) -> Result<()> {
        let info = self
            .client()?
            .subscribe(id, since, Some(PAGE_ITEMS))
            .await?;
        let mut state = self.lock();
        if let Some(open) = state.open.get_mut(id) {
            let conversation = &mut open.conversation;
            conversation.info.model = info.model;
            conversation.info.reasoning = info.reasoning;
            conversation.info.policy_default = info.policy_default;
            conversation.state = info.state;
            open.changed = true;
        }
        drop(state);
        self.wake.notify_one();
        Ok(())
    }
}

/// Runs `f` with the current client on the core's runtime.
async fn with_client<T, F, Fut>(shared: &Arc<Shared>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(Client) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>> + Send + 'static,
{
    let client = shared.client()?;
    run(f(client)).await
}

fn session_id(id: String) -> SessionId {
    SessionId(id)
}

impl AgentConnection {
    pub(crate) async fn open(
        relay: RelayHost,
        instance: InstanceId,
        identity: Arc<ClientIdentity>,
    ) -> Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            relay,
            instance: instance.clone(),
            identity,
            state: Mutex::new(State {
                agent: Agent {
                    instance: instance.to_string(),
                    workspace: None,
                    version: None,
                    default_model: None,
                },
                client: None,
                epoch: 0,
                closed: false,
                link: None,
                sessions: Vec::new(),
                sessions_changed: false,
                open: HashMap::new(),
                queue: Vec::new(),
            }),
            wake: Notify::new(),
        });
        let opened = shared.clone();
        run(async move {
            let (client, notifications) = opened.connect().await?;
            opened.attach(client, notifications);
            opened.refresh_sessions().await
        })
        .await?;
        Ok(Arc::new(Self { shared }))
    }
}

#[uniffi::export]
impl AgentConnection {
    /// The agent as it introduced itself.
    pub fn agent(&self) -> Agent {
        self.shared.lock().agent.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.lock().client.is_some()
    }

    /// Checks that the link still answers, as after the app was in the
    /// background or the device changed networks, when a dead link would
    /// only show at the heartbeat's timeout. Returns whether it answered in
    /// time; when it did not, the link counts as lost, so `next_updates`
    /// says so and the app reconnects at once.
    pub async fn check_link(self: Arc<Self>, timeout_ms: u64) -> bool {
        let shared = self.shared.clone();
        run(async move {
            let (client, epoch) = {
                let state = shared.lock();
                match &state.client {
                    Some(client) => (client.clone(), state.epoch),
                    None => return false,
                }
            };
            let answer =
                tokio::time::timeout(Duration::from_millis(timeout_ms), client.list_sessions())
                    .await;
            let mut state = shared.lock();
            let alive = match answer {
                Ok(Ok(sessions)) => {
                    state.sessions = sessions;
                    state.sessions_changed = true;
                    true
                }
                // The agent answered, if only with a refusal.
                Ok(Err(ClientError::Api(_))) => true,
                Ok(Err(_)) | Err(_) => false,
            };
            if !alive && state.epoch == epoch && !state.closed {
                state.epoch += 1;
                state.client = None;
                state.link = Some((false, Some("The agent stopped answering.".into())));
            }
            drop(state);
            shared.wake.notify_one();
            alive
        })
        .await
    }

    /// Waits for what changed since the last call and returns it, or `None`
    /// once the connection is closed for good. Call it in a loop.
    pub async fn next_updates(self: Arc<Self>) -> Option<Vec<AgentUpdate>> {
        let shared = self.shared.clone();
        run(async move {
            loop {
                {
                    let mut state = shared.lock();
                    let updates = state.take_updates();
                    if !updates.is_empty() {
                        return Some(updates);
                    }
                    if state.closed {
                        return None;
                    }
                }
                shared.wake.notified().await;
            }
        })
        .await
    }

    /// Makes a new link after the old one was lost, and catches the open
    /// conversations up on what they missed.
    pub async fn reconnect(self: Arc<Self>) -> Result<()> {
        let shared = self.shared.clone();
        run(async move {
            if shared.lock().closed {
                return Err(LoomError::Closed);
            }
            let (client, notifications) = match shared.connect().await {
                Ok(link) => link,
                Err(err) => {
                    shared.lock().link = Some((false, Some(err.to_string())));
                    shared.wake.notify_one();
                    return Err(err);
                }
            };
            shared.attach(client, notifications);
            shared.refresh_sessions().await?;
            let open: Vec<(SessionId, u64)> = shared
                .lock()
                .open
                .iter()
                .map(|(id, open)| (id.clone(), open.conversation.last_seq))
                .collect();
            for (id, since) in open {
                shared.subscribe(&id, Some(since)).await?;
            }
            Ok(())
        })
        .await
    }

    /// Closes the connection for good; `next_updates` then returns `None`.
    pub fn disconnect(&self) {
        let mut state = self.shared.lock();
        state.closed = true;
        state.client = None;
        state.epoch += 1;
        drop(state);
        self.shared.wake.notify_one();
    }

    pub async fn refresh_sessions(self: Arc<Self>) -> Result<()> {
        let shared = self.shared.clone();
        run(async move { shared.refresh_sessions().await }).await
    }

    /// Starts a session, with the agent's default model unless `model` names
    /// one.
    pub async fn create_session(self: Arc<Self>, model: Option<String>) -> Result<SessionSummary> {
        let shared = self.shared.clone();
        let info = with_client(&self.shared, move |client| async move {
            Ok(client.create_session(model.as_deref()).await?)
        })
        .await?;
        let summary = SessionSummary::from(&info);
        shared.lock().session_updated(info);
        shared.wake.notify_one();
        Ok(summary)
    }

    /// Renames a session; no title brings back the one from its first
    /// message.
    pub async fn rename_session(
        self: Arc<Self>,
        session: String,
        title: Option<String>,
    ) -> Result<()> {
        let info = with_client(&self.shared, move |client| async move {
            Ok(client.rename_session(&session_id(session), title).await?)
        })
        .await?;
        self.shared.lock().session_updated(info);
        self.shared.wake.notify_one();
        Ok(())
    }

    pub async fn archive_session(self: Arc<Self>, session: String, archived: bool) -> Result<()> {
        let info = with_client(&self.shared, move |client| async move {
            Ok(client
                .archive_session(&session_id(session), archived)
                .await?)
        })
        .await?;
        self.shared.lock().session_updated(info);
        self.shared.wake.notify_one();
        Ok(())
    }

    pub async fn delete_session(self: Arc<Self>, session: String) -> Result<()> {
        with_client(&self.shared, move |client| async move {
            Ok(client.delete_session(&session_id(session)).await?)
        })
        .await
    }

    /// Follows a session's conversation; its updates then come from
    /// `next_updates`, the first with every item.
    pub async fn open_session(self: Arc<Self>, session: String) -> Result<()> {
        let shared = self.shared.clone();
        run(async move {
            let id = session_id(session);
            let known = |shared: &Shared| shared.lock().sessions.iter().any(|s| s.id == id);
            if !known(&shared) {
                shared.refresh_sessions().await?;
            }
            {
                let mut state = shared.lock();
                if let Some(open) = state.open.get_mut(&id) {
                    open.sent = None;
                    open.changed = true;
                    drop(state);
                    shared.wake.notify_one();
                    return Ok(());
                }
                let info = state
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .cloned()
                    .ok_or_else(|| LoomError::invalid(format!("there is no session {id}")))?;
                // Before subscribing: the backlog comes as events at once.
                state.open.insert(id.clone(), Open::new(info));
            }
            let subscribed = shared.subscribe(&id, None).await;
            if subscribed.is_err() {
                shared.lock().open.remove(&id);
            }
            subscribed
        })
        .await
    }

    /// Fetches the items before the ones an open session's conversation
    /// holds. Returns whether there are still older ones.
    pub async fn load_earlier(self: Arc<Self>, session: String) -> Result<bool> {
        let shared = self.shared.clone();
        run(async move {
            let id = session_id(session);
            let before = {
                let state = shared.lock();
                let open = state
                    .open
                    .get(&id)
                    .ok_or_else(|| LoomError::invalid(format!("session {id} is not open")))?;
                match open.conversation.first_seq() {
                    Some(first) if open.conversation.has_older => first,
                    _ => return Ok(false),
                }
            };
            let history = shared.client()?.history(&id, before, PAGE_ITEMS).await?;
            let mut state = shared.lock();
            let Some(open) = state.open.get_mut(&id) else {
                return Ok(false);
            };
            // A page that no longer fits, as after the conversation started
            // over, changes nothing.
            if open.conversation.prepend(history) {
                open.changed = true;
            }
            let has_older = open.conversation.has_older;
            drop(state);
            shared.wake.notify_one();
            Ok(has_older)
        })
        .await
    }

    /// Stops following a session's conversation.
    pub async fn close_session(self: Arc<Self>, session: String) -> Result<()> {
        let id = session_id(session);
        if self.shared.lock().open.remove(&id).is_none() {
            return Ok(());
        }
        with_client(&self.shared, move |client| async move {
            Ok(client.unsubscribe(&id).await?)
        })
        .await
    }

    /// Sends a message: it starts a turn when none runs, and otherwise waits
    /// as `delivery` says.
    pub async fn send_prompt(
        self: Arc<Self>,
        session: String,
        text: String,
        images: Vec<ImageData>,
        delivery: Delivery,
    ) -> Result<()> {
        let mut content = Vec::new();
        if !text.trim().is_empty() {
            content.push(ContentPart::text(text));
        }
        content.extend(images.into_iter().map(|image| ContentPart::Image {
            mime_type: image.mime_type,
            data: image.data,
        }));
        if content.is_empty() {
            return Err(LoomError::invalid("the message is empty"));
        }
        with_client(&self.shared, move |client| async move {
            Ok(client
                .send_message(&session_id(session), content, delivery.into())
                .await?)
        })
        .await
    }

    /// Takes back a waiting message, to be edited and sent again.
    pub async fn take_back(self: Arc<Self>, session: String, id: u64) -> Result<TakenBack> {
        with_client(&self.shared, move |client| async move {
            Ok(client
                .cancel_pending(&session_id(session), id)
                .await?
                .into())
        })
        .await
    }

    /// Stops the session's turn; `false` when it was not running one.
    pub async fn interrupt(self: Arc<Self>, session: String) -> Result<bool> {
        with_client(&self.shared, move |client| async move {
            Ok(client.interrupt(&session_id(session)).await?)
        })
        .await
    }

    pub async fn resolve_approval(
        self: Arc<Self>,
        session: String,
        call_id: String,
        decision: ApprovalDecision,
    ) -> Result<()> {
        with_client(&self.shared, move |client| async move {
            Ok(client
                .resolve_approval(&session_id(session), &call_id, decision.into())
                .await?)
        })
        .await
    }

    /// The models of the providers the agent can use.
    pub async fn list_models(self: Arc<Self>) -> Result<Vec<Model>> {
        with_client(&self.shared, move |client| async move {
            let providers = client.list_providers().await?;
            let mut models = Vec::new();
            for provider in providers.iter().filter(|p| p.enabled) {
                match client.list_models(provider.provider.as_str()).await {
                    Ok(list) => models.extend(list.iter().map(Model::from)),
                    Err(err) if provider.signed_in() => return Err(err.into()),
                    Err(_) => {}
                }
            }
            Ok(models)
        })
        .await
    }

    /// Switches the session's model; none goes back to the agent's default.
    pub async fn set_model(self: Arc<Self>, session: String, model: Option<String>) -> Result<()> {
        let info = with_client(&self.shared, move |client| async move {
            Ok(client
                .set_model(&session_id(session), model.as_deref())
                .await?)
        })
        .await?;
        self.shared.lock().session_updated(info);
        self.shared.wake.notify_one();
        Ok(())
    }

    pub async fn set_reasoning(
        self: Arc<Self>,
        session: String,
        reasoning: Option<Reasoning>,
    ) -> Result<()> {
        let info = with_client(&self.shared, move |client| async move {
            Ok(client
                .set_reasoning(&session_id(session), reasoning.map(Into::into))
                .await?)
        })
        .await?;
        self.shared.lock().session_updated(info);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Summarizes the session's earlier turns to free up context.
    pub async fn compact_session(self: Arc<Self>, session: String) -> Result<()> {
        with_client(&self.shared, move |client| async move {
            Ok(client.compact_session(&session_id(session)).await?)
        })
        .await
    }

    /// Empties the session's conversation; its tasks and the agent's notes
    /// stay.
    pub async fn clear_session(self: Arc<Self>, session: String) -> Result<()> {
        with_client(&self.shared, move |client| async move {
            Ok(client.clear_session(&session_id(session)).await?)
        })
        .await
    }

    /// The agent's notes, which its sessions share.
    pub async fn notes(self: Arc<Self>) -> Result<Notes> {
        with_client(&self.shared, move |client| async move {
            Ok(client.notes().await?.into())
        })
        .await
    }

    /// Replaces the agent's notes if they are still at `expected`, the
    /// version that was read; returns the new version.
    pub async fn write_notes(self: Arc<Self>, text: String, expected: String) -> Result<String> {
        let expected =
            parse_version(&expected).ok_or_else(|| LoomError::invalid("not a notes version"))?;
        with_client(&self.shared, move |client| async move {
            let version = client.write_notes(text, expected).await?;
            Ok(version_text(&version))
        })
        .await
    }

    pub async fn tasks(self: Arc<Self>, session: String) -> Result<Vec<TaskItem>> {
        with_client(&self.shared, move |client| async move {
            Ok(task_items(&client.tasks(&session_id(session)).await?.tasks))
        })
        .await
    }

    /// Applies the edits, all or none, and returns the new list.
    pub async fn update_tasks(
        self: Arc<Self>,
        session: String,
        edits: Vec<TaskEdit>,
    ) -> Result<Vec<TaskItem>> {
        let changes = edits.into_iter().map(Into::into).collect();
        with_client(&self.shared, move |client| async move {
            let content = client.update_tasks(&session_id(session), changes).await?;
            Ok(task_items(&content.tasks))
        })
        .await
    }
}
