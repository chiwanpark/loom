use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::auth::{
    AccountInfo, LoginId, LoginInput, LoginMethod, LoginStarted, ModelInfo, ProviderId,
    ProviderStatus,
};
use crate::relay::HostId;
use crate::session::{ContentPart, Decision, SequencedEvent, SessionId, SessionInfo, ToolCallId};

pub const PROTOCOL_VERSION: u16 = 1;

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
}

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
    ConfigReloaded { restart_required: Vec<String> },
    ConfigRejected { message: String },
    ShuttingDown,
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
