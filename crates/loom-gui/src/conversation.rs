use std::collections::HashMap;
use std::sync::Arc;

use gpui::Image;

use loom_client::proto::{
    ContentPart, Decision, SequencedEvent, SessionEvent, SessionInfo, SessionState, ToolCall,
    ToolResult, TurnOutcome, Usage,
};

pub const MAX_TOOL_OUTPUT: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Requested,
    WaitingApproval,
    Running,
    Succeeded,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolItem {
    pub call: ToolCall,
    pub status: ToolStatus,
    pub approval_reason: Option<String>,
    pub decision: Option<Decision>,
    pub output: Vec<u8>,
    pub output_truncated: bool,
    pub result: Option<ToolResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    User {
        text: String,
        images: Vec<Arc<Image>>,
    },
    Assistant {
        text: String,
        reasoning: String,
        streaming: bool,
        model: Option<String>,
    },
    Tool(ToolItem),
    Notice {
        kind: NoticeKind,
        text: String,
    },
}

#[derive(Debug, Clone)]
pub struct Conversation {
    pub info: SessionInfo,
    pub items: Vec<Item>,
    pub state: SessionState,
    pub last_usage: Option<Usage>,
    pub total_usage: Usage,
    pub last_seq: u64,
    pub revision: u64,
    tools: HashMap<String, usize>,
}

impl Conversation {
    pub fn new(info: SessionInfo) -> Self {
        Self {
            state: info.state,
            info,
            items: Vec::new(),
            last_usage: None,
            total_usage: Usage::default(),
            last_seq: 0,
            revision: 0,
            tools: HashMap::new(),
        }
    }

    pub fn title(&self) -> String {
        self.info
            .title
            .clone()
            .unwrap_or_else(|| short_id(&self.info.id.0))
    }

    pub fn pending_approvals(&self) -> impl Iterator<Item = &ToolItem> {
        self.items.iter().filter_map(|item| match item {
            Item::Tool(tool) if tool.status == ToolStatus::WaitingApproval => Some(tool),
            _ => None,
        })
    }

    pub fn is_busy(&self) -> bool {
        self.state != SessionState::Idle
    }

    pub fn apply(&mut self, event: &SequencedEvent) -> bool {
        if event.seq != 0 && event.seq <= self.last_seq {
            return false;
        }
        self.last_seq = self.last_seq.max(event.seq);
        self.revision += 1;
        match &event.event {
            SessionEvent::UserMessage { content } => {
                self.finish_streaming();
                let mut text = Vec::new();
                let mut images = Vec::new();
                for part in content {
                    match part {
                        ContentPart::Text { text: t } => text.push(t.as_str()),
                        ContentPart::Image { mime_type, data } => {
                            images.extend(crate::attachments::decode(mime_type, data))
                        }
                    }
                }
                if self.info.title.is_none() {
                    self.info.title = text
                        .iter()
                        .flat_map(|t| t.lines())
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .map(|l| l.chars().take(80).collect());
                }
                self.items.push(Item::User {
                    text: text.join("\n"),
                    images,
                });
            }
            SessionEvent::AssistantDelta { text } => {
                self.streaming_assistant().0.push_str(text);
            }
            SessionEvent::ReasoningDelta { text } => {
                self.streaming_assistant().1.push_str(text);
            }
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                model,
            } => {
                let streaming = matches!(
                    self.items.last(),
                    Some(Item::Assistant {
                        streaming: true,
                        ..
                    })
                );
                if !streaming {
                    self.items.push(Item::Assistant {
                        text: String::new(),
                        reasoning: String::new(),
                        streaming: true,
                        model: None,
                    });
                }
                if let Some(Item::Assistant {
                    text: t,
                    reasoning: r,
                    streaming,
                    model: m,
                }) = self.items.last_mut()
                {
                    *t = text.clone();
                    if let Some(reasoning) = reasoning {
                        *r = reasoning.clone();
                    }
                    *streaming = false;
                    *m = Some(model.clone());
                }
                if text.is_empty() && reasoning.as_deref().unwrap_or("").is_empty() {
                    self.items.pop();
                }
            }
            SessionEvent::ToolCallRequested { call } => {
                self.finish_streaming();
                self.tools.insert(call.call_id.clone(), self.items.len());
                self.items.push(Item::Tool(ToolItem {
                    call: call.clone(),
                    status: ToolStatus::Requested,
                    approval_reason: None,
                    decision: None,
                    output: Vec::new(),
                    output_truncated: false,
                    result: None,
                }));
            }
            SessionEvent::ApprovalRequired { call_id, reason } => {
                if let Some(tool) = self.tool_mut(call_id) {
                    tool.status = ToolStatus::WaitingApproval;
                    tool.approval_reason = Some(reason.clone());
                }
            }
            SessionEvent::ApprovalResolved { call_id, decision } => {
                if let Some(tool) = self.tool_mut(call_id) {
                    tool.decision = Some(*decision);
                    tool.status = match decision {
                        Decision::Deny => ToolStatus::Denied,
                        _ => ToolStatus::Requested,
                    };
                }
            }
            SessionEvent::ToolCallStarted { call_id } => {
                if let Some(tool) = self.tool_mut(call_id) {
                    tool.status = ToolStatus::Running;
                }
            }
            SessionEvent::ToolOutput { call_id, chunk } => {
                if let Some(tool) = self.tool_mut(call_id) {
                    tool.output.extend_from_slice(&chunk.data);
                    if tool.output.len() > MAX_TOOL_OUTPUT {
                        let excess = tool.output.len() - MAX_TOOL_OUTPUT;
                        tool.output.drain(..excess);
                        tool.output_truncated = true;
                    }
                }
            }
            SessionEvent::ToolCallFinished { call_id, result } => {
                if let Some(tool) = self.tool_mut(call_id) {
                    if tool.status != ToolStatus::Denied {
                        tool.status = if result.is_error {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Succeeded
                        };
                    }
                    tool.result = Some(result.clone());
                }
            }
            SessionEvent::StateChanged { state } => {
                self.state = *state;
                self.info.state = *state;
            }
            SessionEvent::Usage { usage } => {
                self.last_usage = Some(*usage);
                let total = &mut self.total_usage;
                total.input += usage.input;
                total.output += usage.output;
                total.cache_read += usage.cache_read;
                total.cache_write += usage.cache_write;
                total.reasoning += usage.reasoning;
                total.total += usage.total;
            }
            SessionEvent::TurnFinished { outcome } => {
                self.finish_streaming();
                for item in &mut self.items {
                    if let Item::Tool(tool) = item
                        && matches!(
                            tool.status,
                            ToolStatus::Requested
                                | ToolStatus::WaitingApproval
                                | ToolStatus::Running
                        )
                    {
                        tool.status = ToolStatus::Failed;
                    }
                }
                if *outcome == TurnOutcome::Interrupted {
                    self.notice(NoticeKind::Info, "Interrupted.".into());
                }
            }
            SessionEvent::Warning { message } => self.notice(NoticeKind::Warning, message.clone()),
            SessionEvent::Error { message } => self.notice(NoticeKind::Error, message.clone()),
            SessionEvent::ContextCompacted {
                removed_messages,
                summarized,
            } => {
                let how = if *summarized { "summarized" } else { "trimmed" };
                self.notice(
                    NoticeKind::Info,
                    format!("Context {how}: {removed_messages} earlier messages were compacted."),
                );
            }
        }
        true
    }

    fn notice(&mut self, kind: NoticeKind, text: String) {
        self.items.push(Item::Notice { kind, text });
    }

    fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolItem> {
        let index = *self.tools.get(call_id)?;
        match self.items.get_mut(index) {
            Some(Item::Tool(tool)) => Some(tool),
            _ => None,
        }
    }

    fn streaming_assistant(&mut self) -> (&mut String, &mut String) {
        let streaming = matches!(
            self.items.last(),
            Some(Item::Assistant {
                streaming: true,
                ..
            })
        );
        if !streaming {
            self.items.push(Item::Assistant {
                text: String::new(),
                reasoning: String::new(),
                streaming: true,
                model: None,
            });
        }
        match self.items.last_mut() {
            Some(Item::Assistant {
                text, reasoning, ..
            }) => (text, reasoning),
            _ => unreachable!(),
        }
    }

    fn finish_streaming(&mut self) {
        if let Some(Item::Assistant { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
    }
}

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub fn tool_summary(call: &ToolCall) -> String {
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    let field = |name: &str| args.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    let detail = match call.name.as_str() {
        "shell" => field("command"),
        "read" | "write" | "edit" => field("path"),
        _ => None,
    };
    let detail = detail.unwrap_or_else(|| {
        let mut text = call.arguments.replace('\n', " ");
        if text.chars().count() > 120 {
            text = text.chars().take(120).collect::<String>() + "…";
        }
        text
    });
    let first = detail.lines().next().unwrap_or_default();
    format!("{} {}", call.name, first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_client::proto::{OutputChunk, OutputStream, SessionId};

    fn info() -> SessionInfo {
        SessionInfo {
            id: SessionId("0123456789abcdef".into()),
            model: None,
            reasoning: None,
            policy_default: None,
            state: SessionState::Idle,
            last_seq: 0,
            created_at: 0,
            title: None,
            archived: false,
        }
    }

    fn feed(conv: &mut Conversation, events: Vec<SessionEvent>) {
        for event in events {
            let seq = conv.last_seq + 1;
            conv.apply(&SequencedEvent { seq, event });
        }
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            name: name.into(),
            arguments: args.into(),
            risk: None,
        }
    }

    #[test]
    fn streams_and_finalizes_assistant_text() {
        let mut conv = Conversation::new(info());
        assert_eq!(conv.title(), "01234567");
        feed(
            &mut conv,
            vec![
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text("\n  안녕하세요 world\nmore")],
                },
                SessionEvent::StateChanged {
                    state: SessionState::Thinking,
                },
                SessionEvent::ReasoningDelta { text: "hm".into() },
                SessionEvent::AssistantDelta { text: "Hel".into() },
                SessionEvent::AssistantDelta { text: "lo".into() },
            ],
        );
        assert_eq!(conv.title(), "안녕하세요 world");
        assert!(conv.is_busy());
        assert_eq!(
            conv.items[1],
            Item::Assistant {
                text: "Hello".into(),
                reasoning: "hm".into(),
                streaming: true,
                model: None
            }
        );
        feed(
            &mut conv,
            vec![
                SessionEvent::AssistantMessage {
                    text: "Hello!".into(),
                    reasoning: Some("hmm".into()),
                    model: "p/m".into(),
                },
                SessionEvent::Usage {
                    usage: Usage {
                        input: 10,
                        output: 2,
                        total: 12,
                        ..Usage::default()
                    },
                },
                SessionEvent::StateChanged {
                    state: SessionState::Idle,
                },
                SessionEvent::TurnFinished {
                    outcome: TurnOutcome::Completed,
                },
            ],
        );
        assert_eq!(conv.items.len(), 2);
        assert_eq!(
            conv.items[1],
            Item::Assistant {
                text: "Hello!".into(),
                reasoning: "hmm".into(),
                streaming: false,
                model: Some("p/m".into())
            }
        );
        assert_eq!(conv.total_usage.total, 12);
        assert!(!conv.is_busy());
    }

    #[test]
    fn tracks_tool_calls_and_approvals() {
        let mut conv = Conversation::new(info());
        feed(
            &mut conv,
            vec![
                SessionEvent::ToolCallRequested {
                    call: call("c1", "shell", r#"{"command":"ls -la\npwd"}"#),
                },
                SessionEvent::ApprovalRequired {
                    call_id: "c1".into(),
                    reason: "exec".into(),
                },
            ],
        );
        assert_eq!(conv.pending_approvals().count(), 1);
        feed(
            &mut conv,
            vec![
                SessionEvent::ApprovalResolved {
                    call_id: "c1".into(),
                    decision: Decision::Allow,
                },
                SessionEvent::ToolCallStarted {
                    call_id: "c1".into(),
                },
                SessionEvent::ToolOutput {
                    call_id: "c1".into(),
                    chunk: OutputChunk {
                        stream: OutputStream::Stdout,
                        data: b"a\n".to_vec(),
                    },
                },
                SessionEvent::ToolCallFinished {
                    call_id: "c1".into(),
                    result: ToolResult {
                        content: "a".into(),
                        is_error: false,
                        details: None,
                    },
                },
                SessionEvent::ToolCallRequested {
                    call: call("c2", "write", r#"{"path":"x"}"#),
                },
                SessionEvent::ApprovalRequired {
                    call_id: "c2".into(),
                    reason: "write".into(),
                },
                SessionEvent::ApprovalResolved {
                    call_id: "c2".into(),
                    decision: Decision::Deny,
                },
                SessionEvent::ToolCallFinished {
                    call_id: "c2".into(),
                    result: ToolResult {
                        content: "denied".into(),
                        is_error: true,
                        details: None,
                    },
                },
                SessionEvent::ToolCallRequested {
                    call: call("c3", "read", r#"{"path":"y"}"#),
                },
                SessionEvent::TurnFinished {
                    outcome: TurnOutcome::Interrupted,
                },
            ],
        );
        let tools: Vec<&ToolItem> = conv
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Tool(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(tools[0].status, ToolStatus::Succeeded);
        assert_eq!(tools[0].output, b"a\n");
        assert_eq!(tools[1].status, ToolStatus::Denied);
        assert_eq!(tools[2].status, ToolStatus::Failed);
        assert_eq!(conv.pending_approvals().count(), 0);
        assert!(matches!(
            conv.items.last(),
            Some(Item::Notice {
                kind: NoticeKind::Info,
                ..
            })
        ));
        assert_eq!(tool_summary(&tools[0].call), "shell ls -la");
        assert_eq!(tool_summary(&tools[1].call), "write x");
    }

    #[test]
    fn ignores_replayed_events() {
        let mut conv = Conversation::new(info());
        let event = SequencedEvent {
            seq: 5,
            event: SessionEvent::Warning {
                message: "w".into(),
            },
        };
        assert!(conv.apply(&event));
        assert!(!conv.apply(&event));
        assert_eq!(conv.items.len(), 1);
        assert_eq!(conv.last_seq, 5);
    }

    #[test]
    fn caps_tool_output() {
        let mut conv = Conversation::new(info());
        feed(
            &mut conv,
            vec![SessionEvent::ToolCallRequested {
                call: call("c", "shell", "{}"),
            }],
        );
        for _ in 0..3 {
            feed(
                &mut conv,
                vec![SessionEvent::ToolOutput {
                    call_id: "c".into(),
                    chunk: OutputChunk {
                        stream: OutputStream::Stdout,
                        data: vec![b'x'; MAX_TOOL_OUTPUT / 2],
                    },
                }],
            );
        }
        let Item::Tool(tool) = &conv.items[0] else {
            panic!()
        };
        assert_eq!(tool.output.len(), MAX_TOOL_OUTPUT);
        assert!(tool.output_truncated);
    }
}
