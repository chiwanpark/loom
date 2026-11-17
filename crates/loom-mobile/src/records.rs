//! The plain data that crosses into Kotlin, and its conversions from the
//! protocol and view-model types.

use loom_client::proto::{
    self, AgentInfo, ModelInfo, NotesContent, ReasoningEffort, SessionInfo, SessionState,
    TaskChange, TaskStatus,
};
use loom_view_model::conversation::{
    self, Attachment, Conversation, Item, NoticeKind, ToolItem, ToolStatus, tool_label,
    tool_summary,
};
use loom_view_model::tool_details::{CallDetails, call_details, result_diff};
use serde::{Deserialize, Serialize};

/// The most bytes of a tool's output sent to the app, from its end.
pub const MAX_OUTPUT: usize = 32 * 1024;

/// A host paired through a relay; every agent on it is reachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
pub struct Host {
    /// The name the app shows; unique among the hosts.
    pub name: String,
    pub relay_url: String,
    pub host_id: String,
    pub host_key: String,
    /// The agent the pairing link was for.
    pub instance: Option<String>,
}

/// An agent on a host. Only `instance` is known for an agent that has not
/// been reached yet.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Agent {
    pub instance: String,
    pub workspace: Option<String>,
    pub version: Option<String>,
    pub default_model: Option<String>,
}

impl From<&AgentInfo> for Agent {
    fn from(info: &AgentInfo) -> Self {
        Self {
            instance: info.instance.to_string(),
            workspace: Some(info.workspace.display().to_string()),
            version: Some(info.version.clone()),
            default_model: info.default_model.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SessionStatus {
    Idle,
    Thinking,
    Responding,
    RunningTool,
    WaitingApproval,
    Completed,
}

impl From<SessionState> for SessionStatus {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Idle => Self::Idle,
            SessionState::Thinking => Self::Thinking,
            SessionState::Responding => Self::Responding,
            SessionState::RunningTool => Self::RunningTool,
            SessionState::WaitingApproval => Self::WaitingApproval,
            SessionState::Completed => Self::Completed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Reasoning {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl From<ReasoningEffort> for Reasoning {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::None => Self::None,
            ReasoningEffort::Minimal => Self::Minimal,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
        }
    }
}

impl From<Reasoning> for ReasoningEffort {
    fn from(reasoning: Reasoning) -> Self {
        match reasoning {
            Reasoning::None => Self::None,
            Reasoning::Minimal => Self::Minimal,
            Reasoning::Low => Self::Low,
            Reasoning::Medium => Self::Medium,
            Reasoning::High => Self::High,
            Reasoning::Xhigh => Self::Xhigh,
        }
    }
}

/// What happens to a tool call that no policy rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Policy {
    /// Waits for the user's approval.
    Ask,
    Allow,
    Deny,
}

impl From<proto::PolicyAction> for Policy {
    fn from(action: proto::PolicyAction) -> Self {
        match action {
            proto::PolicyAction::Ask => Self::Ask,
            proto::PolicyAction::Allow => Self::Allow,
            proto::PolicyAction::Deny => Self::Deny,
        }
    }
}

impl From<Policy> for proto::PolicyAction {
    fn from(policy: Policy) -> Self {
        match policy {
            Policy::Ask => Self::Ask,
            Policy::Allow => Self::Allow,
            Policy::Deny => Self::Deny,
        }
    }
}

/// What a notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PushKind {
    /// A tool call waits for approval.
    Approval,
    /// A turn completed.
    Finished,
    /// A turn failed.
    Failed,
}

impl From<proto::push::PushKind> for PushKind {
    fn from(kind: proto::push::PushKind) -> Self {
        match kind {
            proto::push::PushKind::Approval => Self::Approval,
            proto::push::PushKind::Finished => Self::Finished,
            proto::push::PushKind::Failed => Self::Failed,
        }
    }
}

/// A notification from an agent, opened.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PushNotice {
    /// The name of the paired host it came from.
    pub host: String,
    pub instance: String,
    pub workspace: String,
    pub session: String,
    pub title: Option<String>,
    pub kind: PushKind,
    /// The tool call that waits, the start of the answer, or the error.
    pub detail: Option<String>,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

impl PushNotice {
    pub fn of(host: &str, notice: proto::push::PushNotice) -> Self {
        Self {
            host: host.to_owned(),
            instance: notice.instance.0,
            workspace: notice.workspace,
            session: notice.session.0,
            title: notice.title,
            kind: notice.kind.into(),
            detail: notice.detail,
            at: notice.at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SessionSummary {
    pub id: String,
    /// The title, or the start of the session ID while it has none.
    pub title: String,
    pub model: Option<String>,
    pub reasoning: Option<Reasoning>,
    pub status: SessionStatus,
    pub archived: bool,
    /// Seconds since the Unix epoch.
    pub created_at: u64,
}

impl From<&SessionInfo> for SessionSummary {
    fn from(info: &SessionInfo) -> Self {
        Self {
            id: info.id.0.clone(),
            title: info
                .title
                .clone()
                .unwrap_or_else(|| loom_view_model::conversation::short_id(&info.id.0)),
            model: info.model.clone(),
            reasoning: info.reasoning.map(Reasoning::from),
            status: info.state.into(),
            archived: info.archived,
            created_at: info.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Model {
    /// What to pass to `set_model`, as in `anthropic/claude-sonnet-4-5`.
    pub model_ref: String,
    pub display_name: String,
    pub provider: String,
}

impl From<&ModelInfo> for Model {
    fn from(model: &ModelInfo) -> Self {
        Self {
            model_ref: model.model_ref(),
            display_name: model.display_name.clone(),
            provider: model.provider.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImageData {
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ToolState {
    Requested,
    WaitingApproval,
    Running,
    Succeeded,
    Failed,
    Denied,
}

impl From<ToolStatus> for ToolState {
    fn from(status: ToolStatus) -> Self {
        match status {
            ToolStatus::Requested => Self::Requested,
            ToolStatus::WaitingApproval => Self::WaitingApproval,
            ToolStatus::Running => Self::Running,
            ToolStatus::Succeeded => Self::Succeeded,
            ToolStatus::Failed => Self::Failed,
            ToolStatus::Denied => Self::Denied,
        }
    }
}

/// A tool call's arguments in the form that says the most about them.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ToolArguments {
    /// Nothing beyond what the summary says, as for a read.
    None,
    /// Text such as a shell command or the content to write, in `language`
    /// (an extension such as `rs`, or `bash`, `json`, `text`).
    Code { text: String, language: String },
    /// The changes of an edit, as unified diff lines.
    Diff { text: String },
}

impl From<CallDetails> for ToolArguments {
    fn from(details: CallDetails) -> Self {
        match details {
            CallDetails::None => Self::None,
            CallDetails::Code { text, language } => Self::Code { text, language },
            CallDetails::Diff(text) => Self::Diff { text },
        }
    }
}

/// What a busy session is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Activity {
    /// The model streams its reasoning, or has not sent output yet.
    Thinking,
    /// The model streams its answer: text or tool calls.
    Responding,
    /// Tool calls are running.
    RunningTools { count: u32 },
    /// Tool calls wait for the user's approval.
    WaitingApproval { count: u32 },
}

impl From<conversation::Activity> for Activity {
    fn from(activity: conversation::Activity) -> Self {
        use conversation::Activity as A;
        match activity {
            A::Thinking => Self::Thinking,
            A::Responding => Self::Responding,
            A::RunningTools(tools) => Self::RunningTools {
                count: tools.len() as u32,
            },
            A::WaitingApproval(count) => Self::WaitingApproval {
                count: count as u32,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

impl From<NoticeKind> for NoticeLevel {
    fn from(kind: NoticeKind) -> Self {
        match kind {
            NoticeKind::Info => Self::Info,
            NoticeKind::Warning => Self::Warning,
            NoticeKind::Error => Self::Error,
        }
    }
}

/// A block of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ConversationItem {
    User {
        text: String,
        images: Vec<ImageData>,
    },
    Assistant {
        text: String,
        reasoning: String,
        streaming: bool,
    },
    Tool {
        call_id: String,
        name: String,
        /// One line on what the call does, as in `shell cargo test`.
        summary: String,
        /// The tool's name with its timeout, if any.
        label: String,
        /// The arguments, to show with the output; the diff replaces them
        /// once there is one.
        arguments: ToolArguments,
        state: ToolState,
        approval_reason: Option<String>,
        /// The end of the output, at most [`MAX_OUTPUT`] bytes.
        output: String,
        output_truncated: bool,
        /// What the tool returned to the model.
        result: Option<String>,
        /// The change an edit or an overwrite made, as unified diff lines.
        diff: Option<String>,
        is_error: bool,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
}

impl From<&Item> for ConversationItem {
    fn from(item: &Item) -> Self {
        match item {
            Item::User { text, images } => Self::User {
                text: text.clone(),
                images: images.iter().map(image).collect(),
            },
            Item::Assistant {
                text,
                reasoning,
                streaming,
            } => Self::Assistant {
                text: text.clone(),
                reasoning: reasoning.clone(),
                streaming: *streaming,
            },
            Item::Tool(tool) => tool_item(tool),
            Item::Notice { kind, text } => Self::Notice {
                level: (*kind).into(),
                text: text.clone(),
            },
        }
    }
}

fn image(attachment: &Attachment) -> ImageData {
    ImageData {
        mime_type: attachment.mime_type.clone(),
        data: attachment.data.to_vec(),
    }
}

fn tool_item(tool: &ToolItem) -> ConversationItem {
    let start = tool.output.len().saturating_sub(MAX_OUTPUT);
    let diff = result_diff(tool.result.as_ref().and_then(|r| r.details.as_ref()));
    ConversationItem::Tool {
        call_id: tool.call.call_id.clone(),
        name: tool.call.name.clone(),
        summary: tool_summary(&tool.call),
        label: tool_label(&tool.call),
        arguments: call_details(&tool.call, diff.is_some()).into(),
        state: tool.status.into(),
        approval_reason: tool.approval_reason.clone(),
        output: String::from_utf8_lossy(&tool.output[start..]).into_owned(),
        output_truncated: tool.output_truncated || start > 0,
        result: tool.result.as_ref().map(|r| r.content.clone()),
        diff,
        is_error: tool.result.as_ref().is_some_and(|r| r.is_error),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChangedItem {
    pub index: u32,
    /// Stays the same for the item when older items are put before it.
    pub key: u64,
    pub item: ConversationItem,
}

/// What changed in an open conversation since the last update. With
/// `reset`, `items` holds every item and replaces the list; otherwise the
/// app cuts its list to `len` items and replaces or appends `items`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConversationUpdate {
    pub session: String,
    pub title: String,
    pub status: SessionStatus,
    /// What a busy session is doing.
    pub activity: Option<Activity>,
    pub model: Option<String>,
    pub reasoning: Option<Reasoning>,
    /// The session's own default for tool calls no rule matches; none
    /// follows the agent's configuration.
    pub policy: Option<Policy>,
    /// Tokens in the context after the last response.
    pub context_tokens: Option<u64>,
    pub pending_approvals: u32,
    /// Messages sent while a turn ran, waiting to be delivered, in order.
    pub waiting: Vec<WaitingMessage>,
    /// Whether the session has items before these, which `load_earlier`
    /// fetches.
    pub has_older: bool,
    /// The key of the first user message that `rewind_session` can go back
    /// to while the session is idle: a compaction summarized the ones before.
    pub rewind_from: u64,
    pub reset: bool,
    pub len: u32,
    pub items: Vec<ChangedItem>,
}

impl ConversationUpdate {
    /// The update for the items that changed after `revision`; all of them
    /// with `reset`.
    pub fn of(conversation: &Conversation, revision: u64, reset: bool) -> Self {
        let revisions = conversation.item_revisions();
        let keys = conversation.item_keys();
        let items = conversation
            .items
            .iter()
            .enumerate()
            .filter(|(ix, _)| reset || revisions.get(*ix).is_none_or(|r| *r > revision))
            .map(|(ix, item)| ChangedItem {
                index: ix as u32,
                key: keys.get(ix).copied().unwrap_or_default(),
                item: item.into(),
            })
            .collect();
        Self {
            session: conversation.info.id.0.clone(),
            title: conversation.title(),
            status: conversation.state.into(),
            activity: conversation.activity().map(Activity::from),
            model: conversation.info.model.clone(),
            reasoning: conversation.info.reasoning.map(Reasoning::from),
            policy: conversation.info.policy_default.map(Policy::from),
            context_tokens: conversation.last_usage.map(|u| u.context_tokens()),
            pending_approvals: conversation.pending_approvals().count() as u32,
            waiting: conversation
                .pending
                .iter()
                .map(WaitingMessage::from)
                .collect(),
            has_older: conversation.has_older,
            rewind_from: conversation.rewind_from(),
            reset,
            len: conversation.items.len() as u32,
            items,
        }
    }
}

/// How a message sent while a turn runs goes into the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Delivery {
    /// It starts the next turn, after the running one completes.
    Queue,
    /// It goes into the running turn at its next step.
    Steer,
}

impl From<Delivery> for proto::Delivery {
    fn from(delivery: Delivery) -> Self {
        match delivery {
            Delivery::Queue => Self::Queue,
            Delivery::Steer => Self::Steer,
        }
    }
}

/// A message waiting for the running turn.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WaitingMessage {
    pub id: u64,
    /// Its text, with a note of its images.
    pub text: String,
    pub delivery: Delivery,
}

impl From<&proto::PendingMessage> for WaitingMessage {
    fn from(message: &proto::PendingMessage) -> Self {
        Self {
            id: message.id,
            text: loom_view_model::conversation::pending_text(message),
            delivery: match message.delivery {
                proto::Delivery::Steer => Delivery::Steer,
                proto::Delivery::Queue | proto::Delivery::Now => Delivery::Queue,
            },
        }
    }
}

/// A waiting message taken back, or the message a rewind removed, to be
/// edited and sent again.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TakenBack {
    pub text: String,
    pub images: Vec<ImageData>,
}

impl From<proto::PendingMessage> for TakenBack {
    fn from(message: proto::PendingMessage) -> Self {
        message.content.into()
    }
}

impl From<Vec<proto::ContentPart>> for TakenBack {
    fn from(content: Vec<proto::ContentPart>) -> Self {
        let mut text = Vec::new();
        let mut images = Vec::new();
        for part in content {
            match part {
                proto::ContentPart::Text { text: part } => text.push(part),
                proto::ContentPart::Image { mime_type, data } => {
                    images.push(ImageData { mime_type, data })
                }
            }
        }
        Self {
            text: text.join("\n\n"),
            images,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ApprovalDecision {
    Allow,
    /// Allow this call, and calls like it for the rest of the session.
    AllowForSession,
    Deny,
}

impl From<ApprovalDecision> for proto::Decision {
    fn from(decision: ApprovalDecision) -> Self {
        match decision {
            ApprovalDecision::Allow => Self::Allow,
            ApprovalDecision::AllowForSession => Self::AllowForSession,
            ApprovalDecision::Deny => Self::Deny,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Notes {
    pub text: String,
    /// Pass it back to `write_notes`, which fails if the notes changed since.
    pub version: String,
    pub max_bytes: u64,
}

impl From<NotesContent> for Notes {
    fn from(notes: NotesContent) -> Self {
        Self {
            text: notes.text,
            version: version_text(&notes.version),
            max_bytes: notes.max_bytes,
        }
    }
}

pub fn version_text(version: &proto::FileVersion) -> String {
    version.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// What an agent update installed, and which agents restart.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentUpdateInfo {
    /// What the replaced binary printed for `--version`.
    pub previous: String,
    /// What the new binary prints for `--version`.
    pub version: String,
    /// Where the binary is on the agent's host.
    pub path: String,
    pub restarting: Vec<Agent>,
    /// The agents of another build that keep their version.
    pub not_restarted: Vec<Agent>,
}

impl From<proto::AgentUpdate> for AgentUpdateInfo {
    fn from(update: proto::AgentUpdate) -> Self {
        Self {
            previous: update.previous,
            version: update.version,
            path: update.path,
            restarting: update.restarting.iter().map(Agent::from).collect(),
            not_restarted: update.not_restarted.iter().map(Agent::from).collect(),
        }
    }
}

/// What a session sends to the model besides the conversation.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SystemPromptInfo {
    pub text: String,
    pub warnings: Vec<String>,
    pub tools: Vec<ToolInfo>,
}

/// A tool as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    /// The JSON schema of the arguments, formatted.
    pub parameters: String,
}

impl From<proto::SystemPrompt> for SystemPromptInfo {
    fn from(prompt: proto::SystemPrompt) -> Self {
        Self {
            text: prompt.text,
            warnings: prompt.warnings,
            tools: prompt
                .tools
                .into_iter()
                .map(|tool| ToolInfo {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.parameters,
                })
                .collect(),
        }
    }
}

pub fn parse_version(text: &str) -> Option<proto::FileVersion> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (ix, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[ix * 2..ix * 2 + 2], 16).ok()?;
    }
    Some(proto::FileVersion(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TaskState {
    Created,
    InProgress,
    Done,
}

impl From<TaskStatus> for TaskState {
    fn from(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Created => Self::Created,
            TaskStatus::InProgress => Self::InProgress,
            TaskStatus::Done => Self::Done,
        }
    }
}

impl From<TaskState> for TaskStatus {
    fn from(state: TaskState) -> Self {
        match state {
            TaskState::Created => Self::Created,
            TaskState::InProgress => Self::InProgress,
            TaskState::Done => Self::Done,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TaskItem {
    /// The number the agent calls the task by, as in `#3`.
    pub id: u32,
    pub text: String,
    pub state: TaskState,
}

impl From<&proto::Task> for TaskItem {
    fn from(task: &proto::Task) -> Self {
        Self {
            id: task.id,
            text: task.text.clone(),
            state: task.status.into(),
        }
    }
}

pub fn task_items(tasks: &[proto::Task]) -> Vec<TaskItem> {
    tasks.iter().map(TaskItem::from).collect()
}

/// A change to a task list; a batch of them applies all or none.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum TaskEdit {
    Add {
        text: String,
        state: TaskState,
    },
    Update {
        id: u32,
        text: Option<String>,
        state: Option<TaskState>,
    },
    Remove {
        id: u32,
    },
}

impl From<TaskEdit> for TaskChange {
    fn from(edit: TaskEdit) -> Self {
        match edit {
            TaskEdit::Add { text, state } => Self::Add {
                text,
                status: state.into(),
            },
            TaskEdit::Update { id, text, state } => Self::Update {
                id,
                text,
                status: state.map(TaskStatus::from),
            },
            TaskEdit::Remove { id } => Self::Remove { id },
        }
    }
}

/// Something the app should show, from [`crate::AgentConnection::next_updates`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AgentUpdate {
    /// The link to the agent came up or went down.
    Link {
        connected: bool,
        error: Option<String>,
    },
    /// The agent's sessions, all of them.
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    Conversation {
        update: ConversationUpdate,
    },
    /// The agent's notes changed; read them again if they are shown.
    NotesChanged {
        version: String,
    },
    TasksChanged {
        session: String,
        tasks: Vec<TaskItem>,
    },
    /// The agent's open terminals, oldest first, with where each is open.
    /// Comes after connecting and whenever one opens, closes, or moves.
    Terminals {
        terminals: Vec<crate::terminal::TerminalSummary>,
    },
    /// A message from the agent for the user, such as a rejected
    /// configuration.
    Notice {
        level: NoticeLevel,
        message: String,
    },
}

/// The message for a notice the agent sends every client, or `None` for
/// the notices the core handles itself.
pub fn notice_message(notice: &proto::Notice) -> Option<(NoticeLevel, String)> {
    Some(match notice {
        proto::Notice::ConfigReloaded { restart_required } if restart_required.is_empty() => (
            NoticeLevel::Info,
            "The agent reloaded its configuration.".into(),
        ),
        proto::Notice::ConfigReloaded { restart_required } => (
            NoticeLevel::Info,
            format!(
                "The agent reloaded its configuration; restart it to apply {}.",
                restart_required.join(", ")
            ),
        ),
        proto::Notice::ConfigRejected { message } => (
            NoticeLevel::Error,
            format!(
                "The agent rejected its configuration and keeps the old one: {}",
                message.lines().next().unwrap_or_default()
            ),
        ),
        proto::Notice::ShuttingDown => (NoticeLevel::Warning, "The agent is shutting down.".into()),
        proto::Notice::SessionUpdated(_)
        | proto::Notice::SessionRemoved(_)
        | proto::Notice::NotesChanged { .. }
        | proto::Notice::TasksChanged { .. }
        | proto::Notice::TerminalsChanged(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use loom_client::proto::ToolResultDetails;

    use super::*;

    #[test]
    fn notes_versions_round_trip_as_text() {
        let version = proto::FileVersion(std::array::from_fn(|i| i as u8 * 7));
        let text = version_text(&version);
        assert_eq!(text.len(), 64);
        assert_eq!(parse_version(&text), Some(version));
        assert_eq!(parse_version("abc"), None);
        assert_eq!(parse_version(&"zz".repeat(32)), None);
        assert_eq!(parse_version(&"é".repeat(32)), None);
    }

    #[test]
    fn long_tool_output_is_cut_from_the_start() {
        let call = proto::ToolCall {
            call_id: "c".into(),
            name: "shell".into(),
            arguments: r#"{"command":"yes"}"#.into(),
            risk: None,
        };
        let mut output = vec![b'a'; 10];
        output.extend(vec![b'b'; MAX_OUTPUT]);
        let item = Item::Tool(ToolItem {
            call,
            status: ToolStatus::Running,
            approval_reason: None,
            decision: None,
            output,
            output_truncated: false,
            result: None,
        });
        let ConversationItem::Tool {
            output,
            output_truncated,
            summary,
            ..
        } = ConversationItem::from(&item)
        else {
            panic!();
        };
        assert_eq!(output.len(), MAX_OUTPUT);
        assert!(!output.contains('a') && output_truncated);
        assert!(summary.contains("yes"), "{summary}");
    }

    /// An edit shows its replacements until its result brings the diff of
    /// the file, which takes their place.
    #[test]
    fn an_edit_shows_its_arguments_until_the_diff_arrives() {
        let call = proto::ToolCall {
            call_id: "c".into(),
            name: "edit".into(),
            arguments: r#"{"path":"a.rs","old_text":"a","new_text":"b"}"#.into(),
            risk: None,
        };
        let mut tool = ToolItem {
            call,
            status: ToolStatus::Running,
            approval_reason: None,
            decision: None,
            output: Vec::new(),
            output_truncated: false,
            result: None,
        };
        let parts = |tool: &ToolItem| match ConversationItem::from(&Item::Tool(tool.clone())) {
            ConversationItem::Tool {
                arguments, diff, ..
            } => (arguments, diff),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            parts(&tool),
            (
                ToolArguments::Diff {
                    text: "-a\n+b".into()
                },
                None
            )
        );

        tool.status = ToolStatus::Succeeded;
        tool.result = Some(proto::ToolResult {
            content: "edited a.rs".into(),
            is_error: false,
            details: Some(ToolResultDetails::Edit {
                path: "a.rs".into(),
                diff: "@@ -1 +1 @@\n-a\n+b\n".into(),
                first_changed_line: Some(1),
            }),
        });
        assert_eq!(
            parts(&tool),
            (ToolArguments::None, Some("@@ -1 +1 @@\n-a\n+b\n".into()))
        );
    }
}
