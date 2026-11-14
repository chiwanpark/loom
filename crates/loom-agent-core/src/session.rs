use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::StreamExt;
use loom_llm::{
    AssistantMessage, CompletionChunk, CompletionRequest, Message, ModelCapabilities, ModelRef,
    ReasoningEffort, StopReason, ToolCall as LlmToolCall, ToolDefinition,
};
use loom_proto::{
    ContentPart, Decision, Delivery, PendingMessage, SequencedEvent, SessionEvent, SessionId,
    SessionInfo, SessionState, Task, TaskChange, TasksContent, ToolCall, ToolResult, TurnOutcome,
};
use loom_tools::{FileTracker, LockWaitHook, TasksAccess, ToolContext, ToolError, Workspace};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentNotice;
use crate::compaction;
use crate::convert;
use crate::error::{CoreError, Result};
use crate::events::{EventHub, Subscription};
use crate::instructions::{PromptInputs, build_system_prompt};
use crate::metrics::Metrics;
use crate::notes::{AgentNotes, NOTES_READ_TOOL};
use crate::observer::SessionObserver;
use crate::policy::{self, SessionPolicy};
use crate::runtime::{Runtime, RuntimeCell};
use crate::store::{EventStore, SessionState as StoredState, StoredSession, UsedModel, now_unix};
use crate::tasks::TaskList;
use loom_config::PolicyAction;

const CHARS_PER_TOKEN: usize = 4;
const OUTPUT_CHANNEL: usize = 64;

struct Turn {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

struct State {
    model: Option<ModelRef>,
    reasoning: Option<ReasoningEffort>,
    history: Vec<Message>,
    policy: SessionPolicy,
    turn: Option<Turn>,
    approvals: HashMap<String, oneshot::Sender<Decision>>,
    context_tokens: Option<u64>,
    archived: bool,
    title: Option<String>,
    last_used: Option<UsedModel>,
    /// The task list, saved apart from the rest of the state.
    tasks: TaskList,
    /// Messages sent while a turn ran, waiting to be delivered, in order.
    pending: Vec<PendingMessage>,
    /// The ID of the next waiting message.
    next_pending: u64,
}

/// What became of a message sent to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// It started a turn.
    Started,
    /// It waits, with this ID, for the running turn.
    Pending(u64),
}

struct Core {
    id: SessionId,
    created_at: u64,
    store: Option<EventStore>,
    /// The agent's notes, which the session shares with the others.
    notes: Arc<AgentNotes>,
    metrics: Arc<Metrics>,
    hub: EventHub,
    state: Mutex<State>,
    runtime: RuntimeCell,
    workspace: Arc<Workspace>,
    files: Arc<FileTracker>,
    system_agents_md: PathBuf,
    removed: CancellationToken,
    notices: Option<broadcast::Sender<AgentNotice>>,
}

#[derive(Clone)]
pub struct Session {
    core: Arc<Core>,
}

pub(crate) struct SessionParams {
    pub runtime: RuntimeCell,
    pub workspace: Arc<Workspace>,
    pub system_agents_md: PathBuf,
    pub model: Option<ModelRef>,
    pub store: Option<EventStore>,
    pub notes: Arc<AgentNotes>,
    pub metrics: Arc<Metrics>,
    pub observer: Option<Arc<dyn SessionObserver>>,
    /// Where the session reports changes of its tasks.
    pub notices: Option<broadcast::Sender<AgentNotice>>,
}

enum TurnError {
    Interrupted,
    Failed(String),
}

impl From<CoreError> for TurnError {
    fn from(err: CoreError) -> Self {
        Self::Failed(err.to_string())
    }
}

impl Core {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: SessionEvent) {
        self.metrics.observe(&event);
        self.hub.emit(event);
    }

    fn persist_state(&self, state: &State) {
        if let Some(store) = &self.store {
            store.save_session(
                &self.id,
                self.created_at,
                &StoredState {
                    model: state.model.as_ref().map(|m| m.to_string()),
                    reasoning: state.reasoning,
                    policy: state.policy.clone(),
                    history: state.history.clone(),
                    archived: state.archived,
                    title: state.title.clone(),
                    last_used: state.last_used.clone(),
                    pending: state.pending.clone(),
                },
            );
        }
    }

    fn persist(&self) {
        let state = self.lock();
        self.persist_state(&state);
    }

    /// Tells clients when this request uses another model or effort than the
    /// session's last one did.
    fn note_model(&self, model: &ModelRef, reasoning: Option<ReasoningEffort>) {
        let used = UsedModel {
            model: model.to_string(),
            reasoning,
        };
        let previous = self.lock().last_used.replace(used.clone());
        if previous.is_some_and(|previous| previous != used) {
            self.emit(SessionEvent::ModelChanged {
                model: used.model,
                reasoning: used.reasoning.map(convert::reasoning_to_proto),
            });
        }
    }

    fn tasks_limit(&self) -> u32 {
        self.runtime.current().config.tools.tasks.max_items
    }

    /// Whether clients should hear of a change: not once the session is gone.
    fn notify(&self, notice: impl FnOnce() -> AgentNotice) {
        if let Some(notices) = &self.notices
            && !self.removed.is_cancelled()
        {
            let _ = notices.send(notice());
        }
    }

    /// Applies `changes` to the task list, all or none, during a turn too,
    /// and returns the new list.
    fn update_tasks(&self, changes: Vec<TaskChange>) -> Result<Vec<Task>> {
        let max = self.tasks_limit();
        let mut state = self.lock();
        let mut list = state.tasks.clone();
        list.apply(changes, max)?;
        if list != state.tasks {
            if let Some(store) = &self.store {
                store.save_tasks(&self.id, &list);
            }
            self.notify(|| AgentNotice::TasksChanged {
                session: self.id.clone(),
                tasks: list.tasks.clone(),
            });
            state.tasks = list;
        }
        Ok(state.tasks.tasks.clone())
    }

    /// Starts a turn with `content` as the user's message. The caller holds
    /// the lock and has checked that no turn runs.
    fn start_turn(self: &Arc<Self>, state: &mut State, content: Vec<ContentPart>) {
        state.history.push(Message::User {
            content: content.iter().map(convert::input_content).collect(),
        });
        self.persist_state(state);
        self.emit(SessionEvent::UserMessage { content });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_turn(self.clone(), cancel.clone()));
        state.turn = Some(Turn {
            cancel,
            task: Some(task),
        });
    }

    /// Starts the next turn with the first waiting message, unless a turn
    /// runs or none waits. Tells whether it started one.
    fn start_next_pending(self: &Arc<Self>) -> bool {
        let mut state = self.lock();
        if state.turn.is_some() || state.pending.is_empty() {
            return false;
        }
        let message = state.pending.remove(0);
        self.emit(SessionEvent::PendingRemoved { id: message.id });
        self.start_turn(&mut state, message.content);
        true
    }

    /// Puts the waiting steering messages into the conversation, in order,
    /// and tells whether there were any. Called between the steps of a
    /// turn, where a user message may follow.
    fn deliver_steering(&self) -> bool {
        let mut state = self.lock();
        let (steering, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut state.pending)
            .into_iter()
            .partition(|message| message.delivery == Delivery::Steer);
        state.pending = rest;
        if steering.is_empty() {
            return false;
        }
        for message in &steering {
            state.history.push(Message::User {
                content: message.content.iter().map(convert::input_content).collect(),
            });
        }
        self.persist_state(&state);
        // Under the lock, like a prompt, so clients see them in order.
        for message in steering {
            self.emit(SessionEvent::PendingRemoved { id: message.id });
            self.emit(SessionEvent::UserMessage {
                content: message.content,
            });
        }
        true
    }

    fn model(&self, runtime: &Runtime) -> Result<ModelRef> {
        if let Some(model) = &self.lock().model {
            return Ok(model.clone());
        }
        match &runtime.config.llm.default_model {
            Some(model) => Ok(ModelRef::parse(model)?),
            None => Err(CoreError::NoModel),
        }
    }
}

impl Session {
    pub(crate) fn new(params: SessionParams) -> Self {
        let id = SessionId(uuid::Uuid::new_v4().to_string());
        let observer = params.observer.clone();
        let session = Self::build(
            params,
            id,
            now_unix(),
            StoredState::default(),
            Vec::new(),
            TaskList::default(),
        );
        session.core.persist();
        session.observe(observer);
        session
    }

    pub(crate) fn restore(params: SessionParams, stored: StoredSession) -> Self {
        let StoredSession {
            id,
            created_at,
            mut state,
            events,
            tasks,
            ..
        } = stored;
        let model = state.model.as_deref().and_then(|m| ModelRef::parse(m).ok());
        let unfinished = turn_unfinished(&events);
        let repaired = repair_history(&mut state.history);
        if state.last_used.is_none() {
            // Saved before the agent kept this: the last response's model,
            // with the effort the session has now.
            state.last_used = last_response_model(&events).map(|model| UsedModel {
                model,
                reasoning: state.reasoning,
            });
        }
        let observer = params.observer.clone();
        let session = Self::build(params, id, created_at, state, events, tasks);
        if let Some(model) = model {
            session.core.lock().model = Some(model);
        }
        if unfinished || repaired {
            session.core.emit(SessionEvent::Warning {
                message: "the agent stopped during this turn".into(),
            });
            session.core.emit(SessionEvent::TurnFinished {
                outcome: TurnOutcome::Interrupted,
            });
            session.core.emit(SessionEvent::StateChanged {
                state: SessionState::Idle,
            });
            session.core.persist();
        }
        session.observe(observer);
        session
    }

    /// Reports later events to `observer`; called once the session's stored
    /// history is in place, so replayed and repair events are not reported.
    fn observe(&self, observer: Option<Arc<dyn SessionObserver>>) {
        if let Some(observer) = observer {
            self.core.hub.observe(self.core.id.clone(), observer);
        }
    }

    fn build(
        params: SessionParams,
        id: SessionId,
        created_at: u64,
        state: StoredState,
        events: Vec<SequencedEvent>,
        tasks: TaskList,
    ) -> Self {
        let hub = EventHub::restore(params.store.clone().map(|s| (s, id.clone())), events);
        Self {
            core: Arc::new(Core {
                id,
                created_at,
                store: params.store,
                notes: params.notes,
                metrics: params.metrics,
                hub,
                state: Mutex::new(State {
                    model: params.model,
                    reasoning: state.reasoning,
                    history: state.history,
                    policy: state.policy,
                    turn: None,
                    approvals: HashMap::new(),
                    context_tokens: None,
                    archived: state.archived,
                    title: state.title,
                    last_used: state.last_used,
                    tasks,
                    next_pending: state.pending.iter().map(|m| m.id + 1).max().unwrap_or(1),
                    pending: state.pending,
                }),
                runtime: params.runtime,
                workspace: params.workspace,
                files: Arc::new(FileTracker::default()),
                system_agents_md: params.system_agents_md,
                removed: CancellationToken::new(),
                notices: params.notices,
            }),
        }
    }

    pub fn created_at(&self) -> u64 {
        self.core.created_at
    }

    pub fn id(&self) -> &SessionId {
        &self.core.id
    }

    pub fn info(&self) -> SessionInfo {
        let (model, reasoning, policy_default, title, archived) = {
            let state = self.core.lock();
            (
                state.model.as_ref().map(|m| m.to_string()),
                state.reasoning,
                state.policy.default,
                state.title.clone().or_else(|| title(&state.history)),
                state.archived,
            )
        };
        SessionInfo {
            id: self.core.id.clone(),
            model,
            reasoning: reasoning.map(convert::reasoning_to_proto),
            policy_default: policy_default.map(convert::policy_to_proto),
            created_at: self.core.created_at,
            title,
            archived,
            state: self.core.hub.state(),
            last_seq: self.core.hub.last_seq(),
        }
    }

    pub fn subscribe(&self, since_seq: Option<u64>) -> Subscription {
        self.core.hub.subscribe(since_seq)
    }

    /// Like [`Session::subscribe`], with a new window bounded to the events of
    /// about the last `tail` conversation items; see [`EventHub::subscribe_tail`].
    pub fn subscribe_tail(&self, since_seq: Option<u64>, tail: Option<u32>) -> Subscription {
        self.core.hub.subscribe_tail(since_seq, tail)
    }

    /// The events of about `items` conversation items before `before_seq`.
    pub fn older_events(&self, before_seq: u64, items: u32) -> loom_proto::History {
        self.core.hub.history(before_seq, items)
    }

    /// The session's tasks, in order.
    pub fn tasks(&self) -> Vec<Task> {
        self.core.lock().tasks.tasks.clone()
    }

    /// The task list with its limit, for clients.
    pub fn tasks_content(&self) -> TasksContent {
        TasksContent {
            tasks: self.tasks(),
            max_tasks: self.core.tasks_limit(),
        }
    }

    /// Applies `changes` to the task list, all or none, and returns the new
    /// list. Fails when a change names a task that is gone, leaves a task
    /// without text, or would go over `tools.tasks.max_items`.
    pub fn update_tasks(&self, changes: Vec<TaskChange>) -> Result<TasksContent> {
        Ok(TasksContent {
            tasks: self.core.update_tasks(changes)?,
            max_tasks: self.core.tasks_limit(),
        })
    }

    pub fn history(&self) -> Vec<Message> {
        self.core.lock().history.clone()
    }

    pub fn set_model(&self, model: Option<&str>) -> Result<()> {
        let runtime = self.core.runtime.current();
        let model = model.map(|m| runtime.parse_model(m)).transpose()?;
        self.core.lock().model = model;
        self.core.persist();
        Ok(())
    }

    pub fn set_reasoning(&self, effort: Option<ReasoningEffort>) {
        self.core.lock().reasoning = effort;
        self.core.persist();
    }

    pub fn set_policy_default(&self, default: Option<PolicyAction>) {
        self.core.lock().policy.default = default;
        self.core.persist();
    }

    pub(crate) fn mark_removed(&self) {
        self.core.removed.cancel();
        self.interrupt();
    }

    pub fn removed(&self) -> CancellationToken {
        self.core.removed.clone()
    }

    pub fn is_archived(&self) -> bool {
        self.core.lock().archived
    }

    pub fn set_archived(&self, archived: bool) {
        if archived {
            self.interrupt();
        }
        self.core.lock().archived = archived;
        self.core.persist();
    }

    pub fn set_title(&self, title: Option<String>) {
        let title = title
            .map(|t| t.trim().chars().take(TITLE_CHARS).collect::<String>())
            .filter(|t| !t.is_empty());
        self.core.lock().title = title;
        self.core.persist();
    }

    pub fn is_busy(&self) -> bool {
        self.core.lock().turn.is_some()
    }

    /// Starts a turn with `content`; fails with [`CoreError::Busy`] while
    /// one runs.
    pub fn send_prompt(&self, content: Vec<ContentPart>) -> Result<()> {
        self.send_message(content, Delivery::Now).map(drop)
    }

    /// Sends the user's message: it starts a turn when none runs, and
    /// otherwise waits as `delivery` says; see [`Delivery`].
    pub fn send_message(&self, content: Vec<ContentPart>, delivery: Delivery) -> Result<Sent> {
        let empty = content.iter().all(|part| match part {
            ContentPart::Text { text } => text.trim().is_empty(),
            ContentPart::Image { .. } => false,
        });
        if empty {
            return Err(CoreError::EmptyPrompt);
        }
        let runtime = self.core.runtime.current();
        let model = self.core.model(&runtime)?;
        runtime.providers.get(&model.provider)?;
        let mut state = self.core.lock();
        if state.turn.is_none() {
            self.core.start_turn(&mut state, content);
            return Ok(Sent::Started);
        }
        if delivery == Delivery::Now {
            return Err(CoreError::Busy);
        }
        let id = state.next_pending;
        state.next_pending += 1;
        let message = PendingMessage {
            id,
            content,
            delivery,
        };
        state.pending.push(message.clone());
        self.core.persist_state(&state);
        self.core.emit(SessionEvent::PendingAdded { message });
        Ok(Sent::Pending(id))
    }

    /// The messages waiting to be delivered, in order.
    pub fn pending(&self) -> Vec<PendingMessage> {
        self.core.lock().pending.clone()
    }

    /// Takes back a waiting message and returns it.
    pub fn cancel_pending(&self, id: u64) -> Result<PendingMessage> {
        let mut state = self.core.lock();
        let index = state
            .pending
            .iter()
            .position(|message| message.id == id)
            .ok_or(CoreError::UnknownPending(id))?;
        let message = state.pending.remove(index);
        self.core.persist_state(&state);
        self.core.emit(SessionEvent::PendingRemoved { id });
        Ok(message)
    }

    /// Compacts the context now, whatever its size. Like a turn it runs in the
    /// background, makes the session busy, and stops on `interrupt`.
    pub fn compact(&self) -> Result<()> {
        let runtime = self.core.runtime.current();
        let model = self.core.model(&runtime)?;
        runtime.providers.get(&model.provider)?;
        let mut state = self.core.lock();
        if state.turn.is_some() {
            return Err(CoreError::Busy);
        }
        if compaction::plan(&state.history) == compaction::Plan::Nothing {
            return Err(CoreError::NothingToCompact);
        }
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_compaction(self.core.clone(), cancel.clone()));
        state.turn = Some(Turn {
            cancel,
            task: Some(task),
        });
        Ok(())
    }

    /// Starts the conversation over, as in a new session: forgets the history,
    /// the events, the context size, and the files read. Keeps the settings
    /// (model, reasoning effort, policy), the notes and tasks, and the title,
    /// which stays the first prompt's when the session was never renamed.
    pub fn clear(&self) -> Result<()> {
        let mut state = self.core.lock();
        if state.turn.is_some() {
            return Err(CoreError::Busy);
        }
        if state.title.is_none() {
            state.title = title(&state.history);
        }
        state.history.clear();
        state.context_tokens = None;
        state.last_used = None;
        self.core.notes.unsee(&self.core.id);
        self.core.files.clear();
        self.core.persist_state(&state);
        // Under the lock, like a prompt, so no prompt lands between the two.
        self.core.emit(SessionEvent::Cleared);
        // The waiting messages stay, so the new log tells of them again.
        for message in &state.pending {
            self.core.emit(SessionEvent::PendingAdded {
                message: message.clone(),
            });
        }
        Ok(())
    }

    pub fn interrupt(&self) -> bool {
        match &self.core.lock().turn {
            Some(turn) => {
                turn.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn resolve_approval(&self, call_id: &str, decision: Decision) -> Result<()> {
        let sender = self
            .core
            .lock()
            .approvals
            .remove(call_id)
            .ok_or_else(|| CoreError::UnknownApproval(call_id.to_owned()))?;
        sender
            .send(decision)
            .map_err(|_| CoreError::UnknownApproval(call_id.to_owned()))?;
        self.core.emit(SessionEvent::ApprovalResolved {
            call_id: call_id.to_owned(),
            decision,
        });
        Ok(())
    }

    pub async fn wait_idle(&self) {
        let task = self
            .core
            .lock()
            .turn
            .as_mut()
            .and_then(|turn| turn.task.take());
        if let Some(task) = task {
            let _ = task.await;
        }
        while self.is_busy() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub async fn shutdown(&self) {
        self.interrupt();
        self.wait_idle().await;
    }
}

async fn run_turn(core: Arc<Core>, cancel: CancellationToken) {
    let outcome = match turn_loop(&core, &cancel).await {
        Ok(()) => TurnOutcome::Completed,
        Err(TurnError::Interrupted) => TurnOutcome::Interrupted,
        Err(TurnError::Failed(message)) => {
            core.emit(SessionEvent::Error { message });
            TurnOutcome::Failed
        }
    };
    let mut state = core.lock();
    state.turn = None;
    state.approvals.clear();
    core.persist_state(&state);
    // Observers may read the session, so they must not run under its lock.
    drop(state);
    core.emit(SessionEvent::TurnFinished { outcome });
    // Waiting messages go on after a completed turn, and are held after an
    // interrupted or failed one.
    if outcome == TurnOutcome::Completed && core.start_next_pending() {
        return;
    }
    if core.lock().turn.is_none() {
        core.hub.set_state(SessionState::Idle);
    }
}

async fn run_compaction(core: Arc<Core>, cancel: CancellationToken) {
    let result = async {
        let runtime = core.runtime.current();
        let model = core.model(&runtime)?;
        let provider = runtime
            .providers
            .get(&model.provider)
            .map_err(CoreError::from)?;
        let capabilities = runtime
            .models
            .capabilities(provider.as_ref(), &model.model)
            .await;
        compact(&core, &model, &capabilities, provider.as_ref(), &cancel).await
    }
    .await;
    match &result {
        Ok(()) => {}
        Err(TurnError::Interrupted) => core.emit(SessionEvent::Warning {
            message: "compaction was interrupted".into(),
        }),
        Err(TurnError::Failed(message)) => core.emit(SessionEvent::Warning {
            message: format!("could not compact the context: {message}"),
        }),
    }
    let completed = result.is_ok();
    let mut state = core.lock();
    state.turn = None;
    core.persist_state(&state);
    drop(state);
    if completed && core.start_next_pending() {
        return;
    }
    if core.lock().turn.is_none() {
        core.hub.set_state(SessionState::Idle);
    }
}

async fn turn_loop(core: &Arc<Core>, cancel: &CancellationToken) -> Result<(), TurnError> {
    let mut rounds: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(TurnError::Interrupted);
        }
        let runtime = core.runtime.current();
        // Read every round, so that a config reload applies to a long turn.
        let max_rounds = runtime.config.limits.max_rounds_per_turn;
        if max_rounds != 0 && rounds >= max_rounds {
            return Err(TurnError::Failed(format!(
                "stopped after {rounds} model calls in one turn \
                 (limits.max_rounds_per_turn); send a message to go on"
            )));
        }
        let first = rounds == 0;
        rounds += 1;
        let model = core.model(&runtime)?;
        let provider = runtime
            .providers
            .get(&model.provider)
            .map_err(CoreError::from)?;
        let specs = runtime.tools.available_specs().await;
        let prompt = build_system_prompt(&PromptInputs {
            base: runtime.config.instructions.system_prompt.as_deref(),
            system_agents_md: &core.system_agents_md,
            workspace_root: core.workspace.root(),
            max_bytes: runtime.config.instructions.max_bytes.as_usize(),
            tools: &specs,
        })
        .await;
        if first {
            for message in prompt.warnings {
                core.emit(SessionEvent::Warning { message });
            }
        }
        let capabilities = runtime
            .models
            .capabilities(provider.as_ref(), &model.model)
            .await;
        maybe_compact(
            core,
            &runtime,
            &model,
            &capabilities,
            provider.as_ref(),
            cancel,
        )
        .await?;
        let (messages, reasoning) = {
            let state = core.lock();
            (state.history.clone(), state.reasoning)
        };
        core.note_model(&model, reasoning);
        let request = CompletionRequest {
            model: model.model.clone(),
            system: Some(prompt.text),
            messages,
            tools: specs
                .into_iter()
                .map(|spec| ToolDefinition {
                    name: spec.name,
                    description: spec.description,
                    parameters: spec.parameters,
                })
                .collect(),
            reasoning,
            max_output_tokens: None,
            session_id: Some(core.id.0.clone()),
            capabilities,
        };
        core.hub.set_state(SessionState::Thinking);
        let message = tokio::select! {
            result = stream_response(core, provider.as_ref(), request) => result?,
            _ = cancel.cancelled() => return Err(TurnError::Interrupted),
        };
        core.emit(SessionEvent::AssistantMessage {
            text: message.text(),
            reasoning: convert::reasoning_summary(&message),
            model: model.to_string(),
        });
        core.emit(SessionEvent::Usage {
            usage: convert::usage(&message.usage),
        });
        if message.stop_reason == StopReason::Length {
            core.emit(SessionEvent::Warning {
                message: "the response hit the output token limit".into(),
            });
        }
        let calls: Vec<LlmToolCall> = message.tool_calls().cloned().collect();
        {
            let mut state = core.lock();
            let usage = &message.usage;
            state.context_tokens = Some(usage.context_tokens()).filter(|&tokens| tokens > 0);
            state.history.push(Message::Assistant(message));
        }
        core.persist();
        if calls.is_empty() {
            // A steering message sent during the answer goes on the turn.
            if core.deliver_steering() {
                continue;
            }
            return Ok(());
        }
        let max_chars = runtime.config.context.max_tool_output_tokens as usize * CHARS_PER_TOKEN;
        let mut interrupted = false;
        for call in &calls {
            let result = if interrupted {
                convert::error_result("not run: the turn was interrupted")
            } else {
                let (result, cancelled) = run_tool_call(core, &runtime, call, cancel).await;
                interrupted = cancelled || cancel.is_cancelled();
                result
            };
            // The notes are capped by their own limit, and a cut-off read
            // would lose their end if the agent wrote the notes back.
            let content = if call.name == NOTES_READ_TOOL {
                result.content
            } else {
                convert::cap_chars(&result.content, max_chars.max(1))
            };
            core.lock().history.push(Message::ToolResult {
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                content: vec![loom_llm::InputContent::text(content)],
                is_error: result.is_error,
            });
        }
        core.persist();
        if interrupted {
            return Err(TurnError::Interrupted);
        }
        core.deliver_steering();
    }
}

async fn maybe_compact(
    core: &Arc<Core>,
    runtime: &Runtime,
    model: &ModelRef,
    capabilities: &ModelCapabilities,
    provider: &dyn loom_llm::LlmProvider,
    cancel: &CancellationToken,
) -> Result<(), TurnError> {
    let context = &runtime.config.context;
    let window = window_tokens(context, capabilities);
    let limit = window as f64 * context.compaction_threshold;
    let used = {
        let state = core.lock();
        state
            .context_tokens
            .unwrap_or_else(|| compaction::estimate_tokens(&state.history))
    };
    if (used as f64) < limit {
        return Ok(());
    }
    compact(core, model, capabilities, provider, cancel).await
}

/// The context window to compact against: the configured one, else the one
/// the provider reports for the model, else a default.
fn window_tokens(context: &loom_config::ContextConfig, capabilities: &ModelCapabilities) -> u64 {
    context
        .window_tokens
        .or(capabilities.context_window)
        .unwrap_or(loom_config::DEFAULT_WINDOW_TOKENS)
}

/// Summarizes the earlier turns, or trims old tool output when there is nothing
/// to summarize. A failed summary is reported as a warning.
async fn compact(
    core: &Arc<Core>,
    model: &ModelRef,
    capabilities: &ModelCapabilities,
    provider: &dyn loom_llm::LlmProvider,
    cancel: &CancellationToken,
) -> Result<(), TurnError> {
    let history = core.lock().history.clone();
    match compaction::plan(&history) {
        compaction::Plan::Nothing => Ok(()),
        compaction::Plan::Elide => {
            let removed = {
                let mut state = core.lock();
                let removed = compaction::elide_tool_results(&mut state.history);
                state.context_tokens = None;
                // The notes' text may have been in the removed results.
                core.notes.unsee(&core.id);
                core.persist_state(&state);
                removed
            };
            core.emit(SessionEvent::ContextCompacted {
                removed_messages: removed as u32,
                summarized: false,
            });
            Ok(())
        }
        compaction::Plan::Summarize { split } => {
            core.hub.set_state(SessionState::Thinking);
            let mut request = compaction::summary_request(&model.model, &history[..split]);
            request.capabilities = capabilities.clone();
            let summary = tokio::select! {
                result = async {
                    let stream = provider.stream(request).await?;
                    loom_llm::collect(stream).await
                } => result,
                _ = cancel.cancelled() => return Err(TurnError::Interrupted),
            };
            match summary {
                Ok(message) if !message.text().trim().is_empty() => {
                    {
                        let mut state = core.lock();
                        if state.history.len() >= split {
                            compaction::apply_summary(
                                &mut state.history,
                                split,
                                message.text().trim(),
                            );
                        }
                        state.context_tokens = None;
                        core.notes.unsee(&core.id);
                        core.persist_state(&state);
                    }
                    core.emit(SessionEvent::ContextCompacted {
                        removed_messages: split as u32,
                        summarized: true,
                    });
                }
                Ok(_) => core.emit(SessionEvent::Warning {
                    message: "could not compact the context: the summary was empty".into(),
                }),
                Err(err) => core.emit(SessionEvent::Warning {
                    message: format!("could not compact the context: {err}"),
                }),
            }
            Ok(())
        }
    }
}

async fn stream_response(
    core: &Core,
    provider: &dyn loom_llm::LlmProvider,
    request: CompletionRequest,
) -> Result<AssistantMessage, TurnError> {
    let mut stream = provider
        .stream(request)
        .await
        .map_err(|e| TurnError::Failed(e.to_string()))?;
    while let Some(chunk) = stream.next().await {
        match chunk.map_err(|e| TurnError::Failed(e.to_string()))? {
            CompletionChunk::TextDelta { delta, .. } => {
                core.emit(SessionEvent::AssistantDelta { text: delta })
            }
            CompletionChunk::ReasoningDelta { delta, .. } => {
                core.emit(SessionEvent::ReasoningDelta { text: delta })
            }
            CompletionChunk::Done(message) => return Ok(message),
            _ => {}
        }
    }
    Err(TurnError::Failed(
        "the model stream ended without a final message".into(),
    ))
}

async fn run_tool_call(
    core: &Arc<Core>,
    runtime: &Runtime,
    call: &LlmToolCall,
    cancel: &CancellationToken,
) -> (ToolResult, bool) {
    let tool = runtime.tools.get(&call.name).cloned();
    let risk = tool
        .as_ref()
        .map(|t| convert::risk(t.risk(&call.arguments)));
    let call_id = call.call_id.clone();
    core.emit(SessionEvent::ToolCallRequested {
        call: ToolCall {
            call_id: call_id.clone(),
            name: call.name.clone(),
            arguments: match &call.arguments {
                serde_json::Value::String(raw) => raw.clone(),
                other => other.to_string(),
            },
            risk,
        },
    });
    let finish = |result: ToolResult, cancelled: bool| {
        core.emit(SessionEvent::ToolCallFinished {
            call_id: call_id.clone(),
            result: result.clone(),
        });
        (result, cancelled)
    };
    let Some(tool) = tool else {
        return finish(
            convert::error_result(format!("unknown tool: {}", call.name)),
            false,
        );
    };
    if !call.arguments.is_object() {
        let message = match &call.arguments {
            // A provider passes arguments it could not parse as the raw text.
            serde_json::Value::String(raw) => {
                match serde_json::from_str::<serde_json::Value>(raw) {
                    Err(err) => format!(
                        "not run: the tool arguments are not valid JSON ({err}); \
                         check the escaping in strings and send the call again"
                    ),
                    Ok(_) => format!("tool arguments must be a JSON object, got: {raw}"),
                }
            }
            other => format!("tool arguments must be a JSON object, got: {other}"),
        };
        return finish(convert::error_result(message), false);
    }
    // Refused before asking for an approval it could not use.
    if let Some(reason) = tool.blocked().await {
        return finish(
            convert::error_result(format!("{} is unavailable: {reason}", call.name)),
            false,
        );
    }

    let verdict = {
        let state = core.lock();
        policy::evaluate(
            &runtime.config.policy,
            &state.policy,
            &call.name,
            &call.arguments,
        )
    };
    match verdict.action {
        PolicyAction::Deny => {
            return finish(
                convert::error_result(format!("{} ({})", policy::DENIED_BY_POLICY, verdict.reason)),
                false,
            );
        }
        PolicyAction::Ask => {
            let (tx, rx) = oneshot::channel();
            core.lock().approvals.insert(call_id.clone(), tx);
            core.hub.set_state(SessionState::WaitingApproval);
            core.emit(SessionEvent::ApprovalRequired {
                call_id: call_id.clone(),
                reason: verdict.reason,
            });
            let decision = tokio::select! {
                decision = rx => decision.unwrap_or(Decision::Deny),
                _ = cancel.cancelled() => {
                    core.lock().approvals.remove(&call_id);
                    return finish(convert::error_result("cancelled before approval"), true);
                }
            };
            match decision {
                Decision::Deny => {
                    return finish(
                        convert::error_result("the user denied this tool call"),
                        false,
                    );
                }
                Decision::AllowForSession => {
                    let mut state = core.lock();
                    state.policy.allow_for_session(&call.name, &call.arguments);
                    core.persist_state(&state);
                }
                Decision::Allow => {}
            }
        }
        PolicyAction::Allow => {}
    }

    core.emit(SessionEvent::ToolCallStarted {
        call_id: call_id.clone(),
    });
    core.hub.set_state(SessionState::RunningTool);
    let (output_tx, mut output_rx) = mpsc::channel(OUTPUT_CHANNEL);
    let hook_core = core.clone();
    let ctx = ToolContext::new(core.workspace.clone())
        .with_notes(core.notes.access(core.id.clone()))
        .with_tasks(Arc::new(SessionTasks(core.clone())))
        .with_files(core.files.clone())
        .with_cancel(cancel.child_token())
        .with_output(output_tx)
        .with_lock_wait(LockWaitHook::new(move |waiting| {
            hook_core.hub.set_state(if waiting {
                SessionState::WaitingLock
            } else {
                SessionState::RunningTool
            });
        }));
    let args = call.arguments.clone();
    let run = async move {
        let result = tool.call(&ctx, args).await;
        drop(ctx);
        result
    };
    let forward = async {
        while let Some(chunk) = output_rx.recv().await {
            core.emit(SessionEvent::ToolOutput {
                call_id: call_id.clone(),
                chunk: convert::output_chunk(chunk),
            });
        }
    };
    let (result, ()) = tokio::join!(run, forward);
    match result {
        Ok(output) => {
            let cancelled = matches!(
                output.details,
                loom_tools::ToolDetails::Shell {
                    cancelled: true,
                    ..
                }
            );
            finish(convert::tool_result(output), cancelled)
        }
        Err(ToolError::Cancelled) => finish(
            convert::error_result("the tool call was cancelled by the user"),
            true,
        ),
        Err(err) => finish(convert::error_result(err.to_string()), false),
    }
}

/// A session's task list as the tasks tools see it.
struct SessionTasks(Arc<Core>);

impl std::fmt::Debug for SessionTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionTasks").field(&self.0.id).finish()
    }
}

impl TasksAccess for SessionTasks {
    fn read(&self) -> Vec<Task> {
        self.0.lock().tasks.tasks.clone()
    }

    fn update(&self, changes: Vec<TaskChange>) -> loom_tools::Result<Vec<Task>> {
        self.0
            .update_tasks(changes)
            .map_err(|err| ToolError::failed(err.to_string()))
    }
}

const TITLE_CHARS: usize = 80;

pub(crate) fn title(history: &[Message]) -> Option<String> {
    history.iter().find_map(|message| match message {
        Message::User { content } => content.iter().find_map(|part| match part {
            loom_llm::InputContent::Text { text } => {
                let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
                Some(line.chars().take(TITLE_CHARS).collect())
            }
            _ => None,
        }),
        _ => None,
    })
}

fn turn_unfinished(events: &[SequencedEvent]) -> bool {
    let last_user = events
        .iter()
        .rposition(|e| matches!(e.event, SessionEvent::UserMessage { .. }));
    let last_finish = events
        .iter()
        .rposition(|e| matches!(e.event, SessionEvent::TurnFinished { .. }));
    match (last_user, last_finish) {
        (Some(user), Some(finish)) => user > finish,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The model of the last response in `events`.
fn last_response_model(events: &[SequencedEvent]) -> Option<String> {
    events.iter().rev().find_map(|e| match &e.event {
        SessionEvent::AssistantMessage { model, .. } => Some(model.clone()),
        _ => None,
    })
}

fn repair_history(history: &mut Vec<Message>) -> bool {
    let Some(index) = history
        .iter()
        .rposition(|m| matches!(m, Message::Assistant(_)))
    else {
        return false;
    };
    let Message::Assistant(message) = &history[index] else {
        return false;
    };
    let answered: std::collections::HashSet<String> = history[index + 1..]
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    let missing: Vec<(String, String)> = message
        .tool_calls()
        .filter(|c| !answered.contains(&c.call_id))
        .map(|c| (c.call_id.clone(), c.name.clone()))
        .collect();
    let repaired = !missing.is_empty();
    for (call_id, tool_name) in missing {
        history.push(Message::ToolResult {
            call_id,
            tool_name,
            content: vec![loom_llm::InputContent::text(
                "not run: the agent stopped during this turn",
            )],
            is_error: true,
        });
    }
    repaired
}
