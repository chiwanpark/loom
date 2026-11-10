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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub details: Option<ToolResultDetails>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    Idle,
    Thinking,
    RunningTool,
    WaitingApproval,
    WaitingLock,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub total: u64,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningEffort>,
    pub policy_default: Option<PolicyAction>,
    pub state: SessionState,
    pub last_seq: u64,
    pub created_at: u64,
    pub title: Option<String>,
    pub archived: bool,
}
