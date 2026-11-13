use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::auth::{
    AccountInfo, LoginId, LoginInput, LoginMethod, LoginStarted, ModelInfo, ProviderId,
    ProviderStatus,
};
use crate::relay::HostId;
use crate::session::{ContentPart, Decision, SequencedEvent, SessionId, SessionInfo, ToolCallId};

pub const PROTOCOL_VERSION: u16 = 8;

pub type RequestId = u64;
pub type TerminalId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstanceId(pub String);

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub instance: InstanceId,
    pub workspace: PathBuf,
    pub pid: u32,
    pub started_at: u64,
    pub version: String,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u16,
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: u16,
    pub agent: AgentInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello(ClientHello),
    Request { id: RequestId, command: Command },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    GetAgentInfo,
    ListProviders,
    ListModels {
        provider: ProviderId,
    },
    StartLogin {
        provider: ProviderId,
        method: LoginMethod,
    },
    CompleteLogin {
        login: LoginId,
        input: LoginInput,
    },
    CancelLogin {
        login: LoginId,
    },
    SetApiKey {
        provider: ProviderId,
        api_key: Option<String>,
    },
    SelectAccount {
        provider: ProviderId,
        account: String,
    },
    Logout {
        provider: ProviderId,
    },
    ListSessions,
    CreateSession {
        model: Option<String>,
    },
    Subscribe {
        session: SessionId,
        since_seq: Option<u64>,
    },
    Unsubscribe {
        session: SessionId,
    },
    SendPrompt {
        session: SessionId,
        content: Vec<ContentPart>,
    },
    Interrupt {
        session: SessionId,
    },
    ResolveApproval {
        session: SessionId,
        call_id: ToolCallId,
        decision: Decision,
    },
    SetModel {
        session: SessionId,
        model: Option<String>,
    },
    SetReasoning {
        session: SessionId,
        effort: Option<ReasoningEffort>,
    },
    SetPolicy {
        session: SessionId,
        default: Option<PolicyAction>,
    },
    ListAgents,
    SpawnAgent {
        workspace: PathBuf,
    },
    StopAgent {
        instance: InstanceId,
    },
    OpenTerminal {
        cols: u16,
        rows: u16,
    },
    TerminalInput {
        terminal: TerminalId,
        data: Vec<u8>,
    },
    ResizeTerminal {
        terminal: TerminalId,
        cols: u16,
        rows: u16,
    },
    CloseTerminal {
        terminal: TerminalId,
    },
    AuthorizeClient {
        pubkey: [u8; 32],
        label: String,
    },
    ListClients,
    RevokeClient {
        label: String,
    },
    GetHostKey,
    GetMetrics,
    ArchiveSession {
        session: SessionId,
        archived: bool,
    },
    DeleteSession {
        session: SessionId,
    },
    GetWorkspaceChanges,
    GetFileDiff {
        path: String,
        old_path: Option<String>,
    },
    RenameSession {
        session: SessionId,
        title: Option<String>,
    },
    ListDirectory {
        path: String,
    },
    ReadFile {
        path: String,
    },
    /// Takes over a terminal after reconnecting: output goes to this connection,
    /// the terminal is resized, and its foreground program is asked to redraw.
    AttachTerminal {
        terminal: TerminalId,
        cols: u16,
        rows: u16,
    },
    /// Compacts the session's context now instead of waiting for the threshold.
    /// Runs in the background like a turn; the result arrives as events.
    CompactSession {
        session: SessionId,
    },
    /// The provider's subscription limits (5-hour and weekly windows, credit
    /// balances). Answered with `Payload::UsageLimits(None)` for providers that
    /// cannot report them.
    GetUsageLimits {
        provider: ProviderId,
    },
    /// Whether the agent exports OpenTelemetry data and how the exports went.
    GetTelemetryStatus,
    /// Replaces the text of an existing file. Fails with
    /// [`ErrorCode::Conflict`] when the file no longer has the `expected`
    /// version, that is, it changed since the client read it.
    WriteFile {
        path: String,
        text: String,
        expected: FileVersion,
    },
    /// Raw bytes of a workspace file, from `offset` on, at most `len` of them
    /// and never more than [`MAX_FILE_CHUNK`]. A client downloads a file by
    /// asking for its chunks in turn. Answered with [`Payload::FileChunk`].
    /// Agents refuse it on relayed connections.
    ReadFileChunk {
        path: String,
        offset: u64,
        len: u32,
    },
    /// Starts the session's conversation over: the agent forgets its history
    /// and events, keeps its settings, and sends [`SessionEvent::Cleared`].
    /// Fails while the session is busy.
    ///
    /// [`SessionEvent::Cleared`]: crate::SessionEvent::Cleared
    ClearSession {
        session: SessionId,
    },
    /// The session's notes: the agent's scratchpad for its work, which stays
    /// when the conversation is compacted or cleared. Answered with
    /// [`Payload::Notes`].
    GetNotes {
        session: SessionId,
    },
    /// Replaces the session's notes, during a turn too; an empty `text`
    /// clears them. Fails with [`ErrorCode::Conflict`] when the notes no
    /// longer have the `expected` version, that is, they changed since the
    /// client read them. Answered with [`Payload::NotesWritten`].
    WriteNotes {
        session: SessionId,
        text: String,
        expected: FileVersion,
    },
    /// The session's task list, which stays when the conversation is
    /// compacted or cleared. Answered with [`Payload::Tasks`].
    GetTasks {
        session: SessionId,
    },
    /// Changes the session's task list, during a turn too. The changes apply
    /// in order and all together: when one fails, for example because its
    /// task is gone, none applies. Answered with [`Payload::Tasks`].
    UpdateTasks {
        session: SessionId,
        changes: Vec<TaskChange>,
    },
}

/// The most bytes one [`Command::ReadFileChunk`] returns.
pub const MAX_FILE_CHUNK: u32 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    Ok,
    AgentInfo(AgentInfo),
    Providers(Vec<ProviderStatus>),
    Models(Vec<ModelInfo>),
    LoginStarted(LoginStarted),
    Account(AccountInfo),
    Sessions(Vec<SessionInfo>),
    Session(SessionInfo),
    Interrupted(bool),
    Agents(Vec<AgentInfo>),
    Terminal(TerminalId),
    HostKey(HostKeyInfo),
    Clients(Vec<AuthorizedClient>),
    Metrics(AgentMetrics),
    WorkspaceChanges(WorkspaceChanges),
    FileDiff(FileDiff),
    Directory(DirectoryListing),
    FileContent(FileContent),
    UsageLimits(Option<UsageLimits>),
    Telemetry(TelemetryStatus),
    /// The version of a file after a write.
    FileWritten(FileVersion),
    FileChunk(FileChunk),
    Notes(NotesContent),
    /// The version of the notes after a write.
    NotesWritten(FileVersion),
    Tasks(TasksContent),
}

/// A session's notes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotesContent {
    pub text: String,
    /// A SHA-256 hash of `text`, for [`Command::WriteNotes`].
    pub version: FileVersion,
    /// The most bytes the notes may hold.
    pub max_bytes: u64,
}

/// A session's task list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksContent {
    pub tasks: Vec<Task>,
    /// The most tasks the list may hold.
    pub max_tasks: u32,
}

/// One task of a session's task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Numbers the task in its session; never reused there.
    pub id: u32,
    pub text: String,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Created,
    InProgress,
    Done,
}

impl TaskStatus {
    /// The status after this one, wrapping around after `Done`.
    pub fn next(self) -> Self {
        match self {
            Self::Created => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Created,
        }
    }

    /// How the status is written: `created`, `in_progress`, or `done`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        }
    }
}

/// A change to a session's task list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskChange {
    /// Adds a task at the end of the list.
    Add {
        text: String,
        status: TaskStatus,
    },
    /// Changes a task's text, its status, or both.
    Update {
        id: u32,
        text: Option<String>,
        status: Option<TaskStatus>,
    },
    Remove {
        id: u32,
    },
}

/// Part of a file's bytes. `size` and `modified` describe the whole file when
/// the chunk was read; a client notices that the file changed between chunks
/// when they differ.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    pub size: u64,
    /// Nanoseconds since the Unix epoch, if the file system reports it.
    pub modified: Option<u64>,
    pub data: Vec<u8>,
}

/// The agent's OpenTelemetry export. Settings apply when the agent starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelemetryStatus {
    /// Telemetry is disabled, or neither metrics nor logs are on.
    Off,
    /// Telemetry is on but the exporter could not start.
    Failed { error: String },
    Exporting {
        /// The configured collector URL; `None` when it comes from the
        /// `OTEL_EXPORTER_OTLP_*` environment or the default.
        endpoint: Option<String>,
        /// `grpc`, `http/protobuf` or `http/json`.
        protocol: String,
        /// One entry per exported signal: `metrics`, `logs`.
        signals: Vec<SignalStatus>,
    },
}

/// How exports of one signal went since the agent started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalStatus {
    pub signal: String,
    pub exports: u64,
    pub failures: u64,
    /// Failures since the last success; 0 when the last export succeeded.
    pub consecutive_failures: u64,
    /// Seconds since the Unix epoch.
    pub last_success_at: Option<u64>,
    pub last_failure: Option<ExportFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportFailure {
    /// Seconds since the Unix epoch.
    pub at: u64,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimits {
    /// Headline limits first.
    pub limits: Vec<UsageLimit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLimit {
    /// Short name such as "5h", "weekly", "weekly (Opus)", or "extra usage".
    pub label: String,
    /// Share used, 0 to 100; `None` when the limit is uncapped.
    pub used_percent: Option<u8>,
    /// Amounts for credit limits, e.g. "$12.34 / $100.00".
    pub detail: Option<String>,
    /// Seconds since the Unix epoch.
    pub resets_at: Option<u64>,
    /// Shown in compact views; the rest are details.
    pub headline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    pub symlink: bool,
    pub size: u64,
    pub ignored: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub text: String,
    pub size: u64,
    pub binary: bool,
    pub truncated: bool,
    /// False when the bytes are not UTF-8 and `text` has replacement
    /// characters, so writing it back would change the file.
    pub valid_utf8: bool,
    /// The version of the bytes that were read, for [`Command::WriteFile`].
    pub version: FileVersion,
}

/// A SHA-256 hash of a file's bytes, used to notice that a file changed
/// between reading and writing it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileVersion(pub [u8; 32]);

impl fmt::Debug for FileVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileVersion(")?;
        for byte in &self.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "…)")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceChanges {
    pub repository: bool,
    pub branch: Option<String>,
    pub files: Vec<FileChange>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub diff: String,
    pub binary: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetrics {
    pub uptime_secs: u64,
    pub sessions: u64,
    pub busy_sessions: u64,
    pub connections: u64,
    pub connections_total: u64,
    pub turns_completed: u64,
    pub turns_failed: u64,
    pub turns_interrupted: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub approvals: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub compactions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKeyInfo {
    pub host: HostId,
    pub static_pubkey: [u8; 32],
    pub relay_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedClient {
    pub label: String,
    pub pubkey: [u8; 32],
    pub added_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    BadRequest,
    NotFound,
    Busy,
    NotAuthenticated,
    Unsupported,
    Provider,
    Config,
    Cancelled,
    VersionMismatch,
    Protocol,
    Internal,
    /// The target changed since the client last read it.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    ConfigReloaded {
        restart_required: Vec<String>,
    },
    ConfigRejected {
        message: String,
    },
    ShuttingDown,
    SessionUpdated(SessionInfo),
    SessionRemoved(SessionId),
    /// A session's notes changed, by the agent or a client. The `version` is
    /// the hash of their new text, as in [`NotesContent`].
    NotesChanged {
        session: SessionId,
        version: FileVersion,
    },
    /// A session's task list changed, by the agent or a client.
    TasksChanged {
        session: SessionId,
        tasks: Vec<Task>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Hello(ServerHello),
    Rejected(ApiError),
    Response {
        id: RequestId,
        result: Result<Payload, ApiError>,
    },
    Event {
        session: SessionId,
        event: SequencedEvent,
    },
    LoginFinished {
        login: LoginId,
        result: Result<AccountInfo, ApiError>,
    },
    Notice(Notice),
    TerminalOutput {
        terminal: TerminalId,
        data: Vec<u8>,
    },
    TerminalExited {
        terminal: TerminalId,
        status: i32,
    },
}
