use std::collections::HashMap;
use std::sync::Arc;

use gpui::Image;

use loom_client::proto::{
    ContentPart, Decision, ReasoningEffort, SequencedEvent, SessionEvent, SessionInfo,
    SessionState, ToolCall, ToolResult, TurnOutcome, Usage,
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
    },
    Tool(ToolItem),
    Notice {
        kind: NoticeKind,
        text: String,
    },
}

/// What a busy session is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Thinking,
    /// The model is streaming its answer.
    Responding,
    /// Summaries of the tool calls that are running.
    RunningTools(Vec<String>),
    /// The number of tool calls waiting for the user.
    WaitingApproval(usize),
    WaitingLock,
}

impl Activity {
    pub fn label(&self) -> String {
        match self {
            Self::Thinking => "Thinking…".into(),
            Self::Responding => "Writing a response…".into(),
            Self::RunningTools(tools) => match tools.as_slice() {
                [] => "Running a tool…".into(),
                [one] => format!("Running {}…", one.lines().next().unwrap_or_default()),
                many => format!("Running {} tools…", many.len()),
            },
            Self::WaitingApproval(0 | 1) => {
                "Waiting for your approval of the tool call above".into()
            }
            Self::WaitingApproval(n) => format!("Waiting for your approval of {n} tool calls"),
            Self::WaitingLock => "Waiting for another session to release the workspace…".into(),
        }
    }
}

/// A short name for a session state, for lists and tooltips.
pub fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "Idle",
        SessionState::Thinking => "Thinking",
        SessionState::RunningTool => "Running a tool",
        SessionState::WaitingApproval => "Waiting for approval",
        SessionState::WaitingLock => "Waiting for the workspace lock",
    }
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
    /// Bumped when a tool call finishes, the only event that can change the
    /// files the session has changed.
    pub changes_revision: u64,
    /// The revision at which each item last changed, parallel to `items`, so
    /// views can tell which items to lay out again.
    item_revisions: Vec<u64>,
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
            changes_revision: 0,
            item_revisions: Vec::new(),
            tools: HashMap::new(),
        }
    }

    /// The revision at which each item last changed.
    pub fn item_revisions(&self) -> &[u64] {
        &self.item_revisions
    }

    fn push(&mut self, item: Item) {
        self.items.push(item);
        self.item_revisions.push(self.revision);
    }

    fn pop(&mut self) {
        self.items.pop();
        self.item_revisions.pop();
    }

    fn touch(&mut self, index: usize) {
        if let Some(revision) = self.item_revisions.get_mut(index) {
            *revision = self.revision;
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

    /// What the session is doing now, shown as the last block of the
    /// conversation; `None` while it is idle.
    pub fn activity(&self) -> Option<Activity> {
        Some(match self.state {
            SessionState::Idle => return None,
            SessionState::Thinking => match self.items.last() {
                Some(Item::Assistant {
                    streaming: true,
                    text,
                    ..
                }) if !text.is_empty() => Activity::Responding,
                _ => Activity::Thinking,
            },
            SessionState::RunningTool => Activity::RunningTools(
                self.items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Tool(tool) if tool.status == ToolStatus::Running => {
                            Some(tool_summary(&tool.call))
                        }
                        _ => None,
                    })
                    .collect(),
            ),
            SessionState::WaitingApproval => {
                Activity::WaitingApproval(self.pending_approvals().count())
            }
            SessionState::WaitingLock => Activity::WaitingLock,
        })
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
                self.push(Item::User {
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
                text, reasoning, ..
            } => {
                let streaming = matches!(
                    self.items.last(),
                    Some(Item::Assistant {
                        streaming: true,
                        ..
                    })
                );
                if !streaming {
                    self.push(Item::Assistant {
                        text: String::new(),
                        reasoning: String::new(),
                        streaming: true,
                    });
                }
                self.touch(self.items.len() - 1);
                if let Some(Item::Assistant {
                    text: t,
                    reasoning: r,
                    streaming,
                }) = self.items.last_mut()
                {
                    *t = text.clone();
                    if let Some(reasoning) = reasoning {
                        *r = reasoning.clone();
                    }
                    *streaming = false;
                }
                if text.is_empty() && reasoning.as_deref().unwrap_or("").is_empty() {
                    self.pop();
                }
            }
            SessionEvent::ToolCallRequested { call } => {
                self.finish_streaming();
                self.tools.insert(call.call_id.clone(), self.items.len());
                self.push(Item::Tool(ToolItem {
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
                self.changes_revision += 1;
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
                for (item, revision) in self.items.iter_mut().zip(&mut self.item_revisions) {
                    if let Item::Tool(tool) = item
                        && matches!(
                            tool.status,
                            ToolStatus::Requested
                                | ToolStatus::WaitingApproval
                                | ToolStatus::Running
                        )
                    {
                        tool.status = ToolStatus::Failed;
                        *revision = self.revision;
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
                // The last reported size predates the compaction; the next response
                // reports the new one.
                self.last_usage = None;
                let how = if *summarized { "summarized" } else { "trimmed" };
                self.notice(
                    NoticeKind::Info,
                    format!("Context {how}: {removed_messages} earlier messages were compacted."),
                );
            }
            SessionEvent::ModelChanged { model, reasoning } => {
                self.finish_streaming();
                self.notice(NoticeKind::Info, model_changed(model, *reasoning));
            }
        }
        true
    }

    fn notice(&mut self, kind: NoticeKind, text: String) {
        self.push(Item::Notice { kind, text });
    }

    fn tool_mut(&mut self, call_id: &str) -> Option<&mut ToolItem> {
        let index = *self.tools.get(call_id)?;
        self.touch(index);
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
            self.push(Item::Assistant {
                text: String::new(),
                reasoning: String::new(),
                streaming: true,
            });
        }
        self.touch(self.items.len() - 1);
        match self.items.last_mut() {
            Some(Item::Assistant {
                text, reasoning, ..
            }) => (text, reasoning),
            _ => unreachable!(),
        }
    }

    fn finish_streaming(&mut self) {
        if let Some(Item::Assistant { streaming, .. }) = self.items.last_mut()
            && *streaming
        {
            *streaming = false;
            self.touch(self.items.len() - 1);
        }
    }
}

/// The notice for a change of model or reasoning effort.
fn model_changed(model: &str, reasoning: Option<ReasoningEffort>) -> String {
    if reasoning == Some(ReasoningEffort::None) {
        return format!("Switched to {model} with reasoning off.");
    }
    let effort = crate::ui::session::reasoning_label(reasoning).to_lowercase();
    format!("Switched to {model} with {effort} reasoning effort.")
}

pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The timeout a shell call asks for, in seconds. Like the shell tool, this
/// takes numbers, numeric strings, and whole floats; zero means none.
pub fn shell_timeout(call: &ToolCall) -> Option<u64> {
    if call.name != "shell" {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(&call.arguments).ok()?;
    let value = args.get("timeout")?;
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|v| *v >= 0. && v.fract() == 0.)
                .map(|v| v as u64)
        })
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|seconds| *seconds > 0)
}

/// 45 -> "45s", 90 -> "1m 30s", 600 -> "10m", 3600 -> "1h", 5400 -> "1h 30m".
fn format_seconds(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    let text: Vec<String> = [(h, "h"), (m, "m"), (s, "s")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    if text.is_empty() {
        "0s".into()
    } else {
        text.join(" ")
    }
}

pub fn tool_summary(call: &ToolCall) -> String {
    let args: serde_json::Value =
        serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
    let field = |name: &str| args.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    let detail = match call.name.as_str() {
        "shell" => field("command"),
        "read" => field("path").map(|path| {
            // The read tool takes numbers or numeric strings.
            let number = |name: &str| {
                args.get(name).and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                })
            };
            match (number("offset"), number("limit")) {
                (Some(offset), Some(limit)) if limit > 0 => {
                    format!("{path} (lines {offset}-{})", offset + limit - 1)
                }
                (None, Some(limit)) if limit > 0 => format!("{path} (lines 1-{limit})"),
                (Some(offset), _) => format!("{path} (from line {offset})"),
                _ => path,
            }
        }),
        "write" | "edit" => field("path"),
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
    format!("{} {first}", tool_label(call))
}

/// The start of a tool's summary: its name, and the timeout of a shell call.
pub fn tool_label(call: &ToolCall) -> String {
    match shell_timeout(call) {
        Some(seconds) => format!("{} (timeout: {})", call.name, format_seconds(seconds)),
        None => call.name.clone(),
    }
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

    /// Applies the events and checks after each one that every item whose
    /// content changed got a new revision, as the message list relies on.
    fn feed(conv: &mut Conversation, events: Vec<SessionEvent>) {
        for event in events {
            let seq = conv.last_seq + 1;
            let (items, revisions) = (conv.items.clone(), conv.item_revisions.clone());
            conv.apply(&SequencedEvent {
                seq,
                event: event.clone(),
            });
            assert_eq!(conv.item_revisions.len(), conv.items.len());
            for (ix, (old, revision)) in items.iter().zip(&revisions).enumerate() {
                if conv.items.get(ix) != Some(old) {
                    assert_ne!(
                        conv.item_revisions.get(ix),
                        Some(revision),
                        "item {ix} changed without a new revision on {event:?}"
                    );
                }
            }
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
    fn shell_timeout_accepts_what_the_tool_accepts() {
        let timeout =
            |args: serde_json::Value| shell_timeout(&call("c", "shell", &args.to_string()));
        assert_eq!(timeout(serde_json::json!({"command": "ls"})), None);
        assert_eq!(
            timeout(serde_json::json!({"command": "ls", "timeout": 30})),
            Some(30)
        );
        assert_eq!(
            timeout(serde_json::json!({"command": "ls", "timeout": "90"})),
            Some(90)
        );
        assert_eq!(
            timeout(serde_json::json!({"command": "ls", "timeout": 60.0})),
            Some(60)
        );
        assert_eq!(
            timeout(serde_json::json!({"command": "ls", "timeout": 0})),
            None
        );
        assert_eq!(
            timeout(serde_json::json!({"command": "ls", "timeout": 1.5})),
            None
        );
        let read = call("c", "read", r#"{"path": "a", "timeout": 5}"#);
        assert_eq!(shell_timeout(&read), None);
    }

    #[test]
    fn seconds_are_formatted_in_units() {
        assert_eq!(format_seconds(45), "45s");
        assert_eq!(format_seconds(90), "1m 30s");
        assert_eq!(format_seconds(600), "10m");
        assert_eq!(format_seconds(3600), "1h");
        assert_eq!(format_seconds(5400), "1h 30m");
        assert_eq!(format_seconds(3661), "1h 1m 1s");
    }

    #[test]
    fn shell_summary_shows_the_timeout() {
        let summary = |args: &str| tool_summary(&call("c", "shell", args));
        assert_eq!(summary(r#"{"command":"ls -la"}"#), "shell ls -la");
        assert_eq!(
            summary(r#"{"command":"cargo build\nls","timeout":1800}"#),
            "shell (timeout: 30m) cargo build"
        );
    }

    #[test]
    fn read_summary_shows_the_line_range() {
        let summary = |args: &str| tool_summary(&call("c", "read", args));
        assert_eq!(summary(r#"{"path":"a.rs"}"#), "read a.rs");
        assert_eq!(
            summary(r#"{"path":"a.rs","offset":776,"limit":12}"#),
            "read a.rs (lines 776-787)"
        );
        assert_eq!(
            summary(r#"{"path":"a.rs","offset":"10","limit":"5"}"#),
            "read a.rs (lines 10-14)"
        );
        assert_eq!(
            summary(r#"{"path":"a.rs","limit":40}"#),
            "read a.rs (lines 1-40)"
        );
        assert_eq!(
            summary(r#"{"path":"a.rs","offset":2937}"#),
            "read a.rs (from line 2937)"
        );
    }

    #[test]
    fn context_size_counts_cached_tokens_and_resets_on_compaction() {
        let mut conv = Conversation::new(info());
        let usage = Usage {
            input: 3,
            output: 437,
            cache_read: 50_000,
            cache_write: 1_200,
            reasoning: 0,
            total: 51_640,
        };
        feed(&mut conv, vec![SessionEvent::Usage { usage }]);
        assert_eq!(conv.last_usage.map(|u| u.context_tokens()), Some(51_640));
        assert_eq!(conv.total_usage.prompt_tokens(), 51_203);
        feed(
            &mut conv,
            vec![SessionEvent::ContextCompacted {
                removed_messages: 4,
                summarized: true,
            }],
        );
        assert_eq!(conv.last_usage, None);
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
            }
        );
        assert_eq!(conv.total_usage.total, 12);
        assert!(!conv.is_busy());
    }

    #[test]
    fn a_model_change_ends_the_reply_and_adds_a_notice() {
        let mut conv = Conversation::new(info());
        feed(
            &mut conv,
            vec![
                SessionEvent::AssistantDelta { text: "Hi".into() },
                SessionEvent::ModelChanged {
                    model: "p/big".into(),
                    reasoning: Some(ReasoningEffort::Xhigh),
                },
                SessionEvent::ModelChanged {
                    model: "p/big".into(),
                    reasoning: None,
                },
                SessionEvent::ModelChanged {
                    model: "p/small".into(),
                    reasoning: Some(ReasoningEffort::None),
                },
            ],
        );
        assert!(matches!(
            conv.items[0],
            Item::Assistant {
                streaming: false,
                ..
            }
        ));
        let notices: Vec<_> = conv.items[1..]
            .iter()
            .map(|item| match item {
                Item::Notice {
                    kind: NoticeKind::Info,
                    text,
                } => text.as_str(),
                other => panic!("not an info notice: {other:?}"),
            })
            .collect();
        assert_eq!(
            notices,
            [
                "Switched to p/big with extra high reasoning effort.",
                "Switched to p/big with default reasoning effort.",
                "Switched to p/small with reasoning off.",
            ]
        );
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
    fn activity_follows_the_session_state() {
        let mut conv = Conversation::new(info());
        assert_eq!(conv.activity(), None);
        let state = |state| SessionEvent::StateChanged { state };
        feed(&mut conv, vec![state(SessionState::Thinking)]);
        assert_eq!(conv.activity(), Some(Activity::Thinking));
        feed(
            &mut conv,
            vec![SessionEvent::AssistantDelta {
                text: "Let me".into(),
            }],
        );
        assert_eq!(conv.activity(), Some(Activity::Responding));
        feed(
            &mut conv,
            vec![
                SessionEvent::ToolCallRequested {
                    call: call("c1", "shell", r#"{"command":"cargo test\nls"}"#),
                },
                SessionEvent::ToolCallStarted {
                    call_id: "c1".into(),
                },
                state(SessionState::RunningTool),
            ],
        );
        let activity = conv.activity().unwrap();
        assert_eq!(
            activity,
            Activity::RunningTools(vec!["shell cargo test".into()])
        );
        assert_eq!(activity.label(), "Running shell cargo test…");
        feed(
            &mut conv,
            vec![
                SessionEvent::ToolCallFinished {
                    call_id: "c1".into(),
                    result: ToolResult {
                        content: String::new(),
                        is_error: false,
                        details: None,
                    },
                },
                SessionEvent::ToolCallRequested {
                    call: call("c2", "write", r#"{"path":"a"}"#),
                },
                SessionEvent::ApprovalRequired {
                    call_id: "c2".into(),
                    reason: "ask".into(),
                },
                state(SessionState::WaitingApproval),
            ],
        );
        let activity = conv.activity().unwrap();
        assert_eq!(activity, Activity::WaitingApproval(1));
        feed(&mut conv, vec![state(SessionState::WaitingLock)]);
        assert_eq!(conv.activity(), Some(Activity::WaitingLock));
        feed(&mut conv, vec![state(SessionState::Idle)]);
        assert_eq!(conv.activity(), None);
    }

    #[test]
    fn activity_labels() {
        assert_eq!(Activity::RunningTools(vec![]).label(), "Running a tool…");
        assert_eq!(
            Activity::RunningTools(vec!["a".into(), "b".into()]).label(),
            "Running 2 tools…"
        );
        assert_eq!(
            Activity::WaitingApproval(2).label(),
            "Waiting for your approval of 2 tool calls"
        );
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
