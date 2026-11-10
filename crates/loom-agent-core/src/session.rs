use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::StreamExt;
use loom_llm::{
    AssistantMessage, CompletionChunk, CompletionRequest, Message, ModelRef, ReasoningEffort,
    StopReason, ToolCall as LlmToolCall, ToolDefinition,
};
use loom_proto::{
    ContentPart, Decision, SequencedEvent, SessionEvent, SessionId, SessionInfo, SessionState,
    ToolCall, ToolResult, TurnOutcome,
};
use loom_tools::{FileTracker, LockWaitHook, ToolContext, ToolError, Workspace};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::compaction;
use crate::convert;
use crate::error::{CoreError, Result};
use crate::events::{EventHub, Subscription};
use crate::instructions::{PromptInputs, build_system_prompt};
use crate::metrics::Metrics;
use crate::policy::{self, SessionPolicy};
use crate::runtime::{Runtime, RuntimeCell};
use crate::store::{EventStore, SessionState as StoredState, StoredSession, now_unix};
use loom_config::PolicyAction;

pub const MAX_ROUNDS_PER_TURN: usize = 200;
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
}

struct Core {
    id: SessionId,
    created_at: u64,
    store: Option<EventStore>,
    metrics: Arc<Metrics>,
    hub: EventHub,
    state: Mutex<State>,
    runtime: RuntimeCell,
    workspace: Arc<Workspace>,
    files: Arc<FileTracker>,
    system_agents_md: PathBuf,
    removed: CancellationToken,
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
    pub metrics: Arc<Metrics>,
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
                },
            );
        }
    }

    fn persist(&self) {
        let state = self.lock();
        self.persist_state(&state);
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
        let session = Self::build(params, id, now_unix(), StoredState::default(), Vec::new());
        session.core.persist();
        session
    }

    pub(crate) fn restore(params: SessionParams, stored: StoredSession) -> Self {
        let StoredSession {
            id,
            created_at,
            mut state,
            events,
        } = stored;
        let model = state.model.as_deref().and_then(|m| ModelRef::parse(m).ok());
        let unfinished = turn_unfinished(&events);
        let repaired = repair_history(&mut state.history);
        let session = Self::build(params, id, created_at, state, events);
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
        session
    }

    fn build(
        params: SessionParams,
        id: SessionId,
        created_at: u64,
        state: StoredState,
        events: Vec<SequencedEvent>,
    ) -> Self {
        let hub = EventHub::restore(params.store.clone().map(|s| (s, id.clone())), events);
        Self {
            core: Arc::new(Core {
                id,
                created_at,
                store: params.store,
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
                }),
                runtime: params.runtime,
                workspace: params.workspace,
                files: Arc::new(FileTracker::default()),
                system_agents_md: params.system_agents_md,
                removed: CancellationToken::new(),
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
                title(&state.history),
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

    pub fn is_busy(&self) -> bool {
        self.core.lock().turn.is_some()
    }

    pub fn send_prompt(&self, content: Vec<ContentPart>) -> Result<()> {
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
        if state.turn.is_some() {
            return Err(CoreError::Busy);
        }
        state.history.push(Message::User {
            content: content.iter().map(convert::input_content).collect(),
        });
        self.core.persist_state(&state);
        self.core.emit(SessionEvent::UserMessage { content });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_turn(self.core.clone(), cancel.clone()));
        state.turn = Some(Turn {
            cancel,
            task: Some(task),
        });
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
    core.emit(SessionEvent::TurnFinished { outcome });
    core.hub.set_state(SessionState::Idle);
}

async fn turn_loop(core: &Arc<Core>, cancel: &CancellationToken) -> Result<(), TurnError> {
    for round in 0..MAX_ROUNDS_PER_TURN {
        if cancel.is_cancelled() {
            return Err(TurnError::Interrupted);
        }
        let runtime = core.runtime.current();
        let model = core.model(&runtime)?;
        let provider = runtime
            .providers
            .get(&model.provider)
            .map_err(CoreError::from)?;
        let specs = runtime.tools.specs();
        let prompt = build_system_prompt(&PromptInputs {
            system_agents_md: &core.system_agents_md,
            workspace_root: core.workspace.root(),
            max_bytes: runtime.config.instructions.max_bytes.as_usize(),
            tools: &specs,
        })
        .await;
        if round == 0 {
            for message in prompt.warnings {
                core.emit(SessionEvent::Warning { message });
            }
        }
        maybe_compact(core, &runtime, &model, provider.as_ref(), cancel).await?;
        let (messages, reasoning) = {
            let state = core.lock();
            (state.history.clone(), state.reasoning)
        };
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
            state.context_tokens = (usage.input > 0).then_some(usage.input + usage.output);
            state.history.push(Message::Assistant(message));
        }
        core.persist();
        if calls.is_empty() {
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
            core.lock().history.push(Message::ToolResult {
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                content: vec![loom_llm::InputContent::text(convert::cap_chars(
                    &result.content,
                    max_chars.max(1),
                ))],
                is_error: result.is_error,
            });
        }
        core.persist();
        if interrupted {
            return Err(TurnError::Interrupted);
        }
    }
    Err(TurnError::Failed(format!(
        "stopped after {MAX_ROUNDS_PER_TURN} model calls in one turn"
    )))
}

async fn maybe_compact(
    core: &Arc<Core>,
    runtime: &Runtime,
    model: &ModelRef,
    provider: &dyn loom_llm::LlmProvider,
    cancel: &CancellationToken,
) -> Result<(), TurnError> {
    let context = &runtime.config.context;
    let limit = context.window_tokens as f64 * context.compaction_threshold;
    let (used, history) = {
        let state = core.lock();
        let used = state
            .context_tokens
            .unwrap_or_else(|| compaction::estimate_tokens(&state.history));
        (used, state.history.clone())
    };
    if (used as f64) < limit {
        return Ok(());
    }
    match compaction::plan(&history) {
        compaction::Plan::Nothing => Ok(()),
        compaction::Plan::Elide => {
            let removed = {
                let mut state = core.lock();
                let removed = compaction::elide_tool_results(&mut state.history);
                state.context_tokens = None;
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
            let request = compaction::summary_request(&model.model, &history[..split]);
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
        return finish(
            convert::error_result(format!(
                "tool arguments must be a JSON object, got: {}",
                call.arguments
            )),
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
                convert::error_result(format!(
                    "this tool call was denied by policy ({})",
                    verdict.reason
                )),
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

const TITLE_CHARS: usize = 80;

fn title(history: &[Message]) -> Option<String> {
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
