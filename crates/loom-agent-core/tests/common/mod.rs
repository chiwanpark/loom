#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use loom_agent_core::{Agent, AgentOptions, Session, Subscription};
use loom_config::{AgentConfig, Dirs, PolicyAction};
use loom_llm::{
    AssistantContent, AssistantMessage, ChunkStream, CompletionChunk, CompletionRequest, LlmError,
    LlmProvider, ProviderId, StopReason, ToolCall, Usage,
};
use loom_proto::{SessionEvent, TurnOutcome};
use serde_json::Value;

pub const PROVIDER: &str = "scripted";
pub const MODEL: &str = "test-model";
pub const MODEL_REF: &str = "scripted/test-model";

pub enum Script {
    Reply(AssistantMessage),
    Fail(String),
    Hang,
    /// Replies once the gate is opened with `notify_one`.
    Gated(Arc<tokio::sync::Notify>, Box<Script>),
}

/// `script`, held back until `gate` is opened.
pub fn gated(gate: &Arc<tokio::sync::Notify>, script: Script) -> Script {
    Script::Gated(gate.clone(), Box::new(script))
}

pub fn text(reply: &str) -> Script {
    Script::Reply(AssistantMessage {
        provider: ProviderId::from_static(PROVIDER),
        model: MODEL.into(),
        content: vec![AssistantContent::Text {
            text: reply.into(),
            provider_data: None,
        }],
        usage: Usage {
            input: 10,
            output: 3,
            total: 13,
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        response_id: None,
    })
}

/// A reply that reasons before it answers with `reply`.
pub fn reasoned(reasoning: &str, reply: &str) -> Script {
    let Script::Reply(mut message) = text(reply) else {
        unreachable!("text makes a reply");
    };
    message.content.insert(
        0,
        AssistantContent::Reasoning {
            summary: reasoning.into(),
            provider_data: None,
        },
    );
    Script::Reply(message)
}

pub fn calls(calls: &[(&str, &str, Value)]) -> Script {
    Script::Reply(AssistantMessage {
        provider: ProviderId::from_static(PROVIDER),
        model: MODEL.into(),
        content: calls
            .iter()
            .map(|(id, name, args)| {
                AssistantContent::ToolCall(ToolCall {
                    call_id: (*id).into(),
                    name: (*name).into(),
                    arguments: args.clone(),
                    provider_data: None,
                })
            })
            .collect(),
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        response_id: None,
    })
}

#[derive(Default)]
pub struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
    pub requests: Mutex<Vec<CompletionRequest>>,
    /// What `list_models` returns.
    pub models: Mutex<Vec<loom_llm::ModelInfo>>,
}

impl ScriptedProvider {
    pub fn new(scripts: Vec<Script>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
        })
    }

    pub fn push(&self, script: Script) {
        self.scripts.lock().unwrap().push_back(script);
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn id(&self) -> ProviderId {
        ProviderId::from_static(PROVIDER)
    }

    async fn list_models(&self) -> loom_llm::Result<Vec<loom_llm::ModelInfo>> {
        Ok(self.models.lock().unwrap().clone())
    }

    async fn stream(&self, request: CompletionRequest) -> loom_llm::Result<ChunkStream> {
        self.requests.lock().unwrap().push(request);
        let mut script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text("(no more scripts)"));
        let mut gate = None;
        if let Script::Gated(notify, inner) = script {
            gate = Some(notify);
            script = *inner;
        }
        let stream = match script {
            Script::Fail(message) => {
                return Err(LlmError::Stream {
                    provider: PROVIDER.into(),
                    code: None,
                    message,
                });
            }
            Script::Hang => futures::stream::pending().boxed(),
            Script::Reply(message) => {
                let mut chunks = vec![Ok(CompletionChunk::Start { response_id: None })];
                for (index, content) in message.content.iter().enumerate() {
                    match content {
                        AssistantContent::Reasoning { summary, .. } => {
                            chunks.push(Ok(CompletionChunk::ReasoningStart { index }));
                            chunks.push(Ok(CompletionChunk::ReasoningDelta {
                                index,
                                delta: summary.clone(),
                            }));
                        }
                        AssistantContent::Text { text: reply, .. } if !reply.is_empty() => {
                            let mid = reply.len() / 2;
                            let mid = (mid..=reply.len())
                                .find(|i| reply.is_char_boundary(*i))
                                .unwrap_or(reply.len());
                            for part in [&reply[..mid], &reply[mid..]] {
                                chunks.push(Ok(CompletionChunk::TextDelta {
                                    index,
                                    delta: part.into(),
                                }));
                            }
                        }
                        AssistantContent::Text { .. } => {}
                        AssistantContent::ToolCall(call) => {
                            chunks.push(Ok(CompletionChunk::ToolCallStart {
                                index,
                                call_id: call.call_id.clone(),
                                name: call.name.clone(),
                            }));
                        }
                    }
                }
                chunks.push(Ok(CompletionChunk::Done(message)));
                futures::stream::iter(chunks).boxed()
            }
            Script::Gated(..) => unreachable!("gates do not nest"),
        };
        Ok(match gate {
            Some(gate) => futures::stream::once(async move {
                gate.notified().await;
                stream
            })
            .flatten()
            .boxed(),
            None => stream,
        })
    }
}

pub struct Harness {
    pub agent: Agent,
    pub provider: Arc<ScriptedProvider>,
    pub dir: tempfile::TempDir,
    pub workspace: PathBuf,
    pub dirs: Dirs,
}

pub fn dirs_in(root: &std::path::Path) -> Dirs {
    Dirs {
        home: root.to_path_buf(),
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
        runtime_dir: None,
    }
}

pub fn test_config(default: PolicyAction) -> AgentConfig {
    let mut config = AgentConfig::default();
    config.llm.default_model = Some(MODEL_REF.into());
    config.policy.default = default;
    config
}

impl Harness {
    pub fn reopen(
        &self,
        config: AgentConfig,
        scripts: Vec<Script>,
    ) -> (Agent, Arc<ScriptedProvider>) {
        let provider = ScriptedProvider::new(scripts);
        let mut options = AgentOptions::new(&self.workspace, config, self.dirs.clone());
        options.providers.push(provider.clone());
        (Agent::new(options).unwrap(), provider)
    }

    pub fn new(config: AgentConfig, scripts: Vec<Script>) -> Self {
        Self::with_workspace(config, scripts, |_| {}, |_| {})
    }

    /// Like [`Harness::new`]; `prepare` fills the workspace before the agent
    /// starts, and `adjust` changes its options.
    pub fn with_workspace(
        config: AgentConfig,
        scripts: Vec<Script>,
        prepare: impl FnOnce(&std::path::Path),
        adjust: impl FnOnce(&mut AgentOptions),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        prepare(&workspace);
        let dirs = dirs_in(dir.path());
        let provider = ScriptedProvider::new(scripts);
        let mut options = AgentOptions::new(&workspace, config, dirs.clone());
        options.providers.push(provider.clone());
        adjust(&mut options);
        let agent = Agent::new(options).unwrap();
        Self {
            agent,
            provider,
            dir,
            workspace,
            dirs,
        }
    }
}

pub async fn next_event(sub: &mut Subscription) -> SessionEvent {
    if !sub.backlog.is_empty() {
        return sub.backlog.remove(0).event;
    }
    tokio::time::timeout(Duration::from_secs(10), sub.live.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
        .event
}

pub async fn collect_until(
    sub: &mut Subscription,
    stop: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let event = next_event(sub).await;
        let done = stop(&event);
        events.push(event);
        if done {
            return events;
        }
    }
}

pub async fn run_turn(
    session: &Session,
    sub: &mut Subscription,
    prompt: &str,
) -> Vec<SessionEvent> {
    session
        .send_prompt(vec![loom_proto::ContentPart::text(prompt)])
        .unwrap();
    collect_until(sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await
}

pub fn outcome(events: &[SessionEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        SessionEvent::TurnFinished { outcome } => Some(*outcome),
        _ => None,
    })
}
