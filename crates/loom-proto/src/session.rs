use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::{PolicyAction, ReasoningEffort};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub type ToolCallId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentPart {
    Text { text: String },
    Image { mime_type: String, data: Vec<u8> },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    ReadOnly,
    Write,
    Exec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: ToolCallId,
    pub name: String,
    pub arguments: String,
    pub risk: Option<Risk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    AllowForSession,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultDetails {
    Read {
        path: PathBuf,
        start_line: u64,
        end_line: u64,
        total_lines: u64,
        truncated: bool,
    },
    Edit {
        path: PathBuf,
        diff: String,
        first_changed_line: Option<u64>,
    },
    Write {
        path: PathBuf,
        bytes: u64,
        created: bool,
    },
    Shell {
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        cancelled: bool,
        truncated: bool,
        total_lines: u64,
        total_bytes: u64,
        full_output_path: Option<PathBuf>,
        duration_ms: u64,
    },
    WriteWithDiff {
        path: PathBuf,
        bytes: u64,
        created: bool,
        diff: String,
    },
    /// The files a patch changed, in the patch's order.
    Patch { files: Vec<PatchedFile> },
}

/// One file a patch changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchedFile {
    /// The file's path after the change; for a deleted file, its old path.
    pub path: PathBuf,
    pub change: FileChangeKind,
    /// A unified diff of the change.
    pub diff: String,
    pub first_changed_line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
    /// Moved from `from`, maybe with changes too.
    Moved {
        from: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub details: Option<ToolResultDetails>,
}

/// What a session does. New variants go last: postcard stores the variant's
/// position, and stored [`SessionEvent::StateChanged`] events must keep
/// decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// The user saw the finished session.
    Idle,
    /// The model works on its answer: it streams its reasoning, or it has
    /// not sent output yet.
    Thinking,
    /// The agent runs tool calls, also while a call waits for the workspace
    /// lock.
    RunningTool,
    /// A tool call waits for the user's approval.
    WaitingApproval,
    /// The model streams its answer: text or tool calls.
    ///
    /// Agents before protocol 15 sent a state for waiting for the workspace
    /// lock at this position. Their stored events read back as this state,
    /// which a later event of the turn always replaces.
    Responding,
    /// The turn completed, but the user did not open the session since. A
    /// client marks it seen with [`crate::Command::MarkSessionSeen`], which
    /// makes it idle; the next turn also ends this state.
    Completed,
}

impl SessionState {
    /// Whether a turn or a compaction runs: the session is neither idle nor
    /// completed.
    pub fn is_busy(self) -> bool {
        !matches!(self, Self::Idle | Self::Completed)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub total: u64,
}

impl Usage {
    /// Every prompt token of the request. `input` counts only uncached tokens;
    /// cache reads and writes are reported separately by the providers.
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Tokens in the context after the response: the whole prompt plus the output.
    pub fn context_tokens(&self) -> u64 {
        self.prompt_tokens() + self.output
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    UserMessage {
        content: Vec<ContentPart>,
    },
    AssistantDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
        reasoning: Option<String>,
        model: String,
    },
    ToolCallRequested {
        call: ToolCall,
    },
    ApprovalRequired {
        call_id: ToolCallId,
        reason: String,
    },
    ApprovalResolved {
        call_id: ToolCallId,
        decision: Decision,
    },
    ToolCallStarted {
        call_id: ToolCallId,
    },
    ToolOutput {
        call_id: ToolCallId,
        chunk: OutputChunk,
    },
    ToolCallFinished {
        call_id: ToolCallId,
        result: ToolResult,
    },
    StateChanged {
        state: SessionState,
    },
    Usage {
        usage: Usage,
    },
    TurnFinished {
        outcome: TurnOutcome,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
    ContextCompacted {
        removed_messages: u32,
        summarized: bool,
    },
    /// The next response comes from another model or with another reasoning
    /// effort than the session's last one. Not sent before a session's first
    /// response. `reasoning` is `None` for the provider's default effort.
    ///
    /// New variants go last: postcard stores the variant's position, and
    /// stored events must keep decoding.
    ModelChanged {
        model: String,
        reasoning: Option<ReasoningEffort>,
    },
    /// The conversation was cleared: clients drop everything shown before it.
    /// It is the first event of a session's log afterwards. Messages still
    /// waiting are added again right after it.
    Cleared,
    /// A message sent while a turn ran waits to be delivered; see
    /// [`Delivery`]. After a turn that was interrupted or failed, waiting
    /// messages stay until the next turn completes.
    PendingAdded {
        message: PendingMessage,
    },
    /// A waiting message left the queue: delivered, with its `UserMessage`
    /// event right after this one, or cancelled.
    PendingRemoved {
        id: u64,
    },
    /// The conversation went back to before the user message of event
    /// `seq`: clients drop the events from `seq` on, and what they showed
    /// for them. Messages still waiting are added again, or removed, right
    /// after it, so that the events before it and these tell which wait.
    Rewound {
        seq: u64,
    },
}

/// How a message a client sends goes into the conversation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    /// Starts a turn; refused while one runs.
    Now,
    /// Starts a turn, or waits for the running one to complete and starts
    /// the next.
    #[default]
    Queue,
    /// Starts a turn, or goes into the running one at its next step: after
    /// the results of the tool calls in progress, or after an answer, which
    /// the turn then goes on from.
    Steer,
}

/// A message waiting to be delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMessage {
    /// Unique within the session.
    pub id: u64,
    pub content: Vec<ContentPart>,
    /// [`Delivery::Queue`] or [`Delivery::Steer`].
    pub delivery: Delivery,
}

/// Applies what `event` changes to the waiting messages. A message added
/// again, as after [`SessionEvent::Rewound`], replaces the one it repeats.
pub fn apply_pending(pending: &mut Vec<PendingMessage>, event: &SessionEvent) {
    match event {
        SessionEvent::PendingAdded { message } => {
            match pending.iter_mut().find(|m| m.id == message.id) {
                Some(existing) => *existing = message.clone(),
                None => pending.push(message.clone()),
            }
        }
        SessionEvent::PendingRemoved { id } => pending.retain(|message| message.id != *id),
        SessionEvent::Cleared => pending.clear(),
        _ => {}
    }
}

impl SessionEvent {
    pub fn is_ephemeral(&self) -> bool {
        matches!(
            self,
            Self::AssistantDelta { .. } | Self::ReasoningDelta { .. } | Self::ToolOutput { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequencedEvent {
    pub seq: u64,
    pub event: SessionEvent,
}

/// The token usage a session's events add up to, so a client that loads
/// only its latest events still knows the totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSoFar {
    /// Every request's usage, summed.
    pub total: Usage,
    /// The last request's usage, for the size of the context; `None` after
    /// a compaction until the next request reports it.
    pub last: Option<Usage>,
}

impl UsageSoFar {
    /// Adds what `event` changes.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Usage { usage } => {
                self.last = Some(*usage);
                let total = &mut self.total;
                total.input += usage.input;
                total.output += usage.output;
                total.cache_read += usage.cache_read;
                total.cache_write += usage.cache_write;
                total.reasoning += usage.reasoning;
                total.total += usage.total;
            }
            // The last reported size predates the compaction; the next
            // response reports the new one.
            SessionEvent::ContextCompacted { .. } => self.last = None,
            SessionEvent::Cleared => *self = Self::default(),
            _ => {}
        }
    }
}

/// Where the events of an [`crate::ServerMessage::Events`] begin, on the
/// first message of a subscription that starts a new window of the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventWindow {
    /// Whether the session has events before the window, to fetch with
    /// [`crate::Command::GetHistory`].
    pub has_older: bool,
    /// The usage of the events before the window.
    pub usage: UsageSoFar,
    /// The messages waiting as of the window's start.
    pub pending: Vec<PendingMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningEffort>,
    pub policy_default: PolicyAction,
    pub comment_guard: bool,
    pub state: SessionState,
    pub last_seq: u64,
    pub created_at: u64,
    pub title: Option<String>,
    pub archived: bool,
    /// The git worktree the session works in; `None` for a session in the
    /// workspace.
    pub worktree: Option<WorktreeInfo>,
}

/// The git worktree of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub branch: String,
    /// The branch that the worktree started from, and that merges go into.
    pub base: String,
    /// The session's root in the worktree.
    pub path: String,
    /// The URL of the pull request of the branch, once there is one.
    pub pull_request: Option<String>,
}

/// How to make the worktree of a new session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeOptions {
    /// The new branch; `None` makes `loom/<session>`.
    pub branch: Option<String>,
    /// The branch or commit to start from; `None` takes the branch that the
    /// workspace has checked out.
    pub base: Option<String>,
}

/// Where the branch of a worktree is against its base.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeStatus {
    /// Commits of the branch that the base does not have.
    pub ahead: u32,
    /// Commits of the base that the branch does not have.
    pub behind: u32,
    /// The worktree has uncommitted changes.
    pub dirty: bool,
    /// The worktree's directory is gone.
    pub missing: bool,
    /// The base has all of the branch's commits, or the branch did not
    /// change since its last merge.
    pub merged: bool,
}

/// How [`Command::MergeWorktree`](crate::Command::MergeWorktree) merges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    /// A merge commit that keeps the commits of the branch.
    Merge,
    /// One new commit with the changes of the branch, and this message.
    Squash { message: String },
}
