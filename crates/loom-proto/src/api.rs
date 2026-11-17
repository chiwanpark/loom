use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::auth::{
    AccountInfo, LoginId, LoginInput, LoginMethod, LoginStarted, ModelInfo, ProviderId,
    ProviderStatus,
};
use crate::relay::HostId;
use crate::session::{
    ContentPart, Decision, Delivery, EventWindow, MergeStrategy, PendingMessage, SequencedEvent,
    SessionId, SessionInfo, ToolCallId, UsageSoFar, WorktreeOptions, WorktreeStatus,
};

pub const PROTOCOL_VERSION: u16 = 17;

pub type RequestId = u64;
pub type TerminalId = u64;
/// A connection to a TCP port that the agent opened for a client; the
/// client picks the number, unique among its connection's tunnels.
pub type TunnelId = u64;
/// A token a client picks at random and sends with the terminals it opens
/// and attaches, so that after a reconnect it can take back a terminal its
/// earlier connection still holds (as when the agent has not yet noticed
/// that connection is gone).
pub type TerminalOwner = u64;
/// A file that a client uploads; the client picks the number, unique among
/// its connection's uploads.
pub type UploadId = u64;

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
        /// Makes the session work in a new git worktree of its own, on a new
        /// branch, outside the workspace.
        worktree: Option<WorktreeOptions>,
    },
    /// Follows a session's events. The agent answers with the session's
    /// info, then sends its events in [`ServerMessage::Events`] batches.
    ///
    /// With `since_seq`, it sends the events after that one, the ones a
    /// client that saw it missed. Without it, or when more than `tail` items
    /// came since, it starts a new window instead: the first batch carries an
    /// [`EventWindow`], and the client drops what it held for the session.
    /// `tail` bounds a new window to the events of about that many of the
    /// latest conversation items; `None` sends all of them.
    Subscribe {
        session: SessionId,
        since_seq: Option<u64>,
        tail: Option<u32>,
    },
    /// The events of about `items` conversation items before `before_seq`,
    /// for a client that loaded only the latest ones.
    GetHistory {
        session: SessionId,
        before_seq: u64,
        items: u32,
    },
    Unsubscribe {
        session: SessionId,
    },
    SendPrompt {
        session: SessionId,
        content: Vec<ContentPart>,
        delivery: Delivery,
    },
    /// Takes back a message that waits to be delivered. Answered with
    /// [`Payload::PendingCancelled`] holding it, for the client to edit;
    /// fails with [`ErrorCode::NotFound`] once it was delivered.
    CancelPending {
        session: SessionId,
        id: u64,
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
    /// Reads the agent's configuration file again and applies it, as the
    /// agent does on its own when it sees the file change. Answered with
    /// [`Payload::ConfigReloaded`]. Fails with [`ErrorCode::Config`] when the
    /// file is rejected; the agent then keeps the configuration it has.
    ReloadConfig,
    /// Opens a terminal attached to this connection. Answered with
    /// [`Payload::Terminal`].
    OpenTerminal {
        cols: u16,
        rows: u16,
        owner: TerminalOwner,
        /// Starts the terminal in the root of this session, such as its
        /// worktree; `None` starts it in the workspace.
        session: Option<SessionId>,
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
    /// The file commands below act in the root of `session`, such as its
    /// worktree, or in the workspace when it is `None`.
    GetWorkspaceChanges {
        session: Option<SessionId>,
    },
    GetFileDiff {
        path: String,
        old_path: Option<String>,
        session: Option<SessionId>,
    },
    RenameSession {
        session: SessionId,
        title: Option<String>,
    },
    ListDirectory {
        path: String,
        session: Option<SessionId>,
    },
    ReadFile {
        path: String,
        session: Option<SessionId>,
    },
    /// Attaches a terminal to this connection: its recent output (up to
    /// [`TERMINAL_REPLAY`] bytes) comes as [`ServerMessage::TerminalOutput`]
    /// before any new output, the terminal is resized, and its foreground
    /// program is asked to redraw. A terminal is attached to one connection
    /// at a time; the one that held it before gets
    /// [`Notice::TerminalsChanged`]. Unless `take_over` is set, it fails with
    /// [`ErrorCode::Conflict`] while a connection of another `owner` holds
    /// the terminal.
    AttachTerminal {
        terminal: TerminalId,
        cols: u16,
        rows: u16,
        owner: TerminalOwner,
        take_over: bool,
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
        session: Option<SessionId>,
    },
    /// Raw bytes of a workspace file, from `offset` on, at most `len` of them
    /// and never more than [`MAX_FILE_CHUNK`]. A client downloads a file by
    /// asking for its chunks in turn. Answered with [`Payload::FileChunk`].
    ReadFileChunk {
        path: String,
        offset: u64,
        len: u32,
        session: Option<SessionId>,
    },
    /// Starts the session's conversation over: the agent forgets its history
    /// and events, keeps its settings, and sends [`SessionEvent::Cleared`].
    /// Fails while the session is busy.
    ///
    /// [`SessionEvent::Cleared`]: crate::SessionEvent::Cleared
    ClearSession {
        session: SessionId,
    },
    /// The agent's notes: a scratchpad for the work in its workspace, which
    /// all of its sessions share and which stays when a conversation is
    /// compacted or cleared. Answered with [`Payload::Notes`].
    GetNotes,
    /// Replaces the agent's notes, during a turn too; an empty `text` clears
    /// them. Fails with [`ErrorCode::Conflict`] when the notes no longer have
    /// the `expected` version, that is, they changed since the client read
    /// them. Answered with [`Payload::NotesWritten`].
    WriteNotes {
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
    /// Sets where the agents of this host send this client's notifications:
    /// a token of a push service such as Firebase Cloud Messaging, or none
    /// to stop them. Only for clients that come through a relay, which
    /// delivers the notifications; the token belongs to the client's key,
    /// so every agent of the host uses it. Answered with [`Payload::Ok`].
    SetPushToken {
        token: Option<String>,
    },
    /// The agent's open terminals, as [`Notice::TerminalsChanged`] lists
    /// them. Answered with [`Payload::Terminals`].
    ListTerminals,
    /// The TCP ports that programs of the agent's user listen on, on the
    /// agent's host. Answered with [`Payload::Ports`].
    ListPorts,
    /// Connects to a TCP port on or near the agent's host, as for a client
    /// that forwards one of its own ports there. `tunnel` is a number the
    /// client picks; it fails with [`ErrorCode::Conflict`] while the
    /// connection has a tunnel of that number. Answered with
    /// [`Payload::Ok`] once connected; then what the port sends comes as
    /// [`ServerMessage::TunnelData`], at most [`TUNNEL_WINDOW`] bytes ahead
    /// of the client's [`Command::TunnelAck`]s, and its end as
    /// [`ServerMessage::TunnelEnd`]. A connection has at most
    /// [`MAX_TUNNELS`] tunnels, and they close with it.
    OpenTunnel {
        tunnel: TunnelId,
        host: String,
        port: u16,
    },
    /// Bytes for the port, at most [`MAX_TUNNEL_DATA`] of them. Answered
    /// with [`Payload::Ok`] once they were written, so a client keeps only a
    /// few of these unanswered.
    TunnelData {
        tunnel: TunnelId,
        data: Vec<u8>,
    },
    /// Tells that the client took `bytes` of the tunnel's data, so the
    /// agent may send that many more.
    TunnelAck {
        tunnel: TunnelId,
        bytes: u32,
    },
    /// The client sends no more bytes: the agent ends its side of the
    /// connection for writing once it wrote the earlier ones. The tunnel
    /// closes when both sides ended.
    TunnelEnd {
        tunnel: TunnelId,
    },
    /// Closes a tunnel in both directions at once.
    CloseTunnel {
        tunnel: TunnelId,
    },
    /// Tells that a person saw the session, for example in a client that
    /// shows it: a [`SessionState::Completed`] session becomes
    /// [`SessionState::Idle`]. Answered with [`Payload::Ok`].
    ///
    /// [`SessionState::Completed`]: crate::SessionState::Completed
    /// [`SessionState::Idle`]: crate::SessionState::Idle
    MarkSessionSeen {
        session: SessionId,
    },
    /// Writes bytes of a file that the client uploads to the workspace, at
    /// most [`MAX_UPLOAD_CHUNK`] of them, at `offset` of the upload's
    /// temporary file. The first chunk of an upload creates that file next
    /// to `path`, whose directory must exist; the chunks may come in any
    /// order. Answered with [`Payload::Ok`]. A connection has at most
    /// [`MAX_UPLOADS`] uploads, and the agent removes their temporary files
    /// when it closes.
    UploadChunk {
        upload: UploadId,
        path: String,
        offset: u64,
        data: Vec<u8>,
        session: Option<SessionId>,
    },
    /// Ends an upload once every chunk was answered: the temporary file,
    /// which must have `size` bytes, replaces the file at the upload's path.
    /// Fails with [`ErrorCode::Conflict`] when that file exists and
    /// `overwrite` is not set; the upload then stays, to finish again or
    /// cancel. Answered with [`Payload::Ok`].
    FinishUpload {
        upload: UploadId,
        size: u64,
        overwrite: bool,
        session: Option<SessionId>,
    },
    /// Stops an upload and removes its temporary file. Answered with
    /// [`Payload::Ok`], also for an upload the agent does not know.
    CancelUpload {
        upload: UploadId,
    },
    /// What the session sends to the model besides the conversation: the
    /// system prompt and the tools, as the next model call of the session
    /// would have them now. Answered with [`Payload::SystemPrompt`].
    GetSystemPrompt {
        session: SessionId,
    },
    /// Installs a release of `loom-agent`, `version` (such as `0.4.0`) or
    /// the latest, in place of the binary that the agent runs. Then every
    /// agent of the host that runs that binary restarts at once, this one
    /// too: their running turns stop, and their connections close. Answered
    /// with [`Payload::AgentUpdated`] before the restart.
    UpdateAgent {
        version: Option<String>,
    },
    /// Takes the conversation back to before the user message of event
    /// `seq`: the agent forgets that message and everything after it, and
    /// sends [`SessionEvent::Rewound`]. The files of the workspace, the
    /// tasks, and the notes stay as they are. Fails while the session is
    /// busy, and for a message that a compaction summarized. Answered with
    /// [`Payload::Rewound`].
    ///
    /// [`SessionEvent::Rewound`]: crate::SessionEvent::Rewound
    Rewind {
        session: SessionId,
        seq: u64,
    },
    /// Where the branch of the session's worktree is against its base.
    /// Answered with [`Payload::WorktreeStatus`].
    GetWorktreeStatus {
        session: SessionId,
    },
    /// Merges the branch of the session's worktree into its base, in the
    /// workspace, which must have the base checked out and no uncommitted
    /// changes. Fails with [`ErrorCode::Conflict`] when the merge has
    /// conflicts; the workspace then stays as it was. With `remove`, removes
    /// the worktree and its branch after the merge, and deletes the session.
    /// Answered with [`Payload::Merged`].
    MergeWorktree {
        session: SessionId,
        strategy: MergeStrategy,
        remove: bool,
    },
    /// Pushes the branch of the session's worktree and opens a pull request
    /// into its base with the GitHub CLI (`gh`) on the agent's host. When the
    /// session has a pull request already, only pushes. Answered with
    /// [`Payload::PullRequest`].
    OpenPullRequest {
        session: SessionId,
        title: String,
        body: String,
        draft: bool,
    },
    /// Removes the session's worktree and its branch, and deletes the
    /// session. Without `force`, fails with [`ErrorCode::Conflict`] when the
    /// worktree has uncommitted changes or commits that were not merged.
    /// [`Command::DeleteSession`] fails in the same way.
    RemoveWorktree {
        session: SessionId,
        force: bool,
    },
}

/// The most bytes in one [`Command::UploadChunk`].
pub const MAX_UPLOAD_CHUNK: u32 = 4 * 1024 * 1024;

/// The most uploads one connection has at once.
pub const MAX_UPLOADS: usize = 16;

/// The most bytes of a tunnel the agent sends ahead of the client's
/// [`Command::TunnelAck`]s.
pub const TUNNEL_WINDOW: u32 = 512 * 1024;

/// The most bytes in one [`Command::TunnelData`] or
/// [`ServerMessage::TunnelData`], so that one fits in a single message of
/// the encrypted transport.
pub const MAX_TUNNEL_DATA: u32 = 60 * 1024;

/// The most tunnels one connection has open at once.
pub const MAX_TUNNELS: usize = 24;

/// A TCP port that a program listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListeningPort {
    pub port: u16,
    /// The address it listens on, such as `127.0.0.1`, `0.0.0.0`, or `::`.
    pub address: String,
    /// The program's name and process id, when the agent can tell.
    pub process: Option<String>,
    pub pid: Option<u32>,
}

impl ListeningPort {
    /// The host a tunnel connects to for this port: the loopback address
    /// for ports that listen on every address.
    pub fn connect_host(&self) -> String {
        match self.address.as_str() {
            "0.0.0.0" | "" => "127.0.0.1".into(),
            "::" => "::1".into(),
            other => other.into(),
        }
    }
}

/// The most bytes of a terminal's recent output that attaching to it replays.
pub const TERMINAL_REPLAY: usize = 128 * 1024;

/// An open terminal of the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: TerminalId,
    pub holder: TerminalHolder,
}

/// Which connection a terminal is attached to, as one connection sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalHolder {
    /// The connection that asks.
    You,
    /// Another connection, of this client.
    Other(TerminalClient),
    /// None: its connection went away, and the agent closes it after
    /// `terminal.detached_timeout` unless a client attaches it.
    Nobody,
}

/// A client as the agent knows it, to tell people where a terminal is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalClient {
    /// The name the client gave in its [`ClientHello`], such as `loom-gui`.
    pub program: String,
    /// The label the client was paired with, for clients that come through
    /// a relay.
    pub label: Option<String>,
}

impl TerminalClient {
    /// How the clients name it to people: its label, or the app it is.
    pub fn describe(&self) -> String {
        if let Some(label) = self.label.as_deref().filter(|l| !l.is_empty()) {
            return label.to_owned();
        }
        match self.program.as_str() {
            "loom-gui" => "the desktop app".into(),
            "loom-android" => "the Android app".into(),
            other => other.to_owned(),
        }
    }
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
    ConfigReloaded(ConfigReload),
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
    History(History),
    /// The message [`Command::CancelPending`] took back.
    PendingCancelled(PendingMessage),
    Terminals(Vec<TerminalInfo>),
    Ports(Vec<ListeningPort>),
    SystemPrompt(SystemPrompt),
    AgentUpdated(AgentUpdate),
    /// The user message that [`Command::Rewind`] removed, for the client to
    /// edit and send again.
    Rewound(Vec<ContentPart>),
    WorktreeStatus(WorktreeStatus),
    /// The commit that a merge made.
    Merged(String),
    /// The URL of a pull request.
    PullRequest(String),
}

/// Older events of a session, from [`Command::GetHistory`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    pub events: Vec<SequencedEvent>,
    /// Whether there are events before these.
    pub has_older: bool,
    /// The usage of the events before these.
    pub usage: UsageSoFar,
}

/// What a [`Command::ReloadConfig`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigReload {
    /// The file differed from the configuration in use, which it replaced.
    /// The sections in `restart_required`, such as `socket`, keep their old
    /// settings until the agent restarts.
    Applied { restart_required: Vec<String> },
    /// The file holds the configuration already in use.
    Unchanged,
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

/// What a [`Command::UpdateAgent`] installed, and which agents restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUpdate {
    /// What the replaced binary printed for `--version`.
    pub previous: String,
    /// What the new binary prints for `--version`.
    pub version: String,
    /// Where the binary is on the agent's host.
    pub path: String,
    /// The agents that restart with the new binary.
    pub restarting: Vec<AgentInfo>,
    /// The agents of another build that run from the same path; they keep
    /// their version until they are restarted by other means.
    pub not_restarted: Vec<AgentInfo>,
}

/// A session's system prompt and tools, from [`Command::GetSystemPrompt`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPrompt {
    pub text: String,
    /// Problems with the instructions, such as an `AGENTS.md` file that
    /// was too large and was truncated.
    pub warnings: Vec<String>,
    /// The tools the model can call, in the order it gets them.
    pub tools: Vec<ToolDefinition>,
}

/// A tool as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// The JSON schema of the tool's arguments, as formatted JSON.
    pub parameters: String,
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
    /// The agent's notes changed, by a session or a client. The `version` is
    /// the hash of their new text, as in [`NotesContent`].
    NotesChanged {
        version: FileVersion,
    },
    /// A session's task list changed, by the agent or a client.
    TasksChanged {
        session: SessionId,
        tasks: Vec<Task>,
    },
    /// The agent's open terminals, sent after [`ServerHello`] and whenever
    /// one opens, closes, or moves to another connection.
    TerminalsChanged(Vec<TerminalInfo>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Hello(ServerHello),
    Rejected(ApiError),
    Response {
        id: RequestId,
        result: Result<Payload, ApiError>,
    },
    /// Events of a subscribed session, in order. `window` is set on the
    /// first batch of a subscription that starts a new window: the client
    /// drops the events it held for the session and starts over with these.
    Events {
        session: SessionId,
        events: Vec<SequencedEvent>,
        window: Option<EventWindow>,
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
    /// Bytes a tunnel's port sent, at most [`MAX_TUNNEL_DATA`] of them.
    TunnelData {
        tunnel: TunnelId,
        data: Vec<u8>,
    },
    /// The port sends no more bytes: it ended its side of the connection,
    /// or, with an `error`, the tunnel failed and is closed.
    TunnelEnd {
        tunnel: TunnelId,
        error: Option<String>,
    },
}
