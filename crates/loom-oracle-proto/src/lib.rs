use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod codespace;
mod git;
mod preset;
mod volume;

pub use codespace::*;
pub use git::*;
pub use preset::*;
pub use volume::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const ORACLE_INSTANCE: &str = "oracle";
pub const DEFAULT_PAIRING_TTL_SECS: u64 = 600;
pub const MAX_PAIRING_TTL_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(ClientHello),
    Request(Request),
    Reply(Reply),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u32,
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OracleError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    OracleInfo,
    Subscribe,
    Unsubscribe,
    ListDevices,
    RenameDevice {
        id: u64,
        label: String,
    },
    RevokeDevice {
        id: u64,
    },
    SetPushToken {
        #[serde(default)]
        token: Option<String>,
    },
    CreateDevicePairing {
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    ListPairings,
    CancelPairing {
        id: String,
    },
    CreateHostPairing {
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    ListHosts,
    RemoveHost {
        name: String,
    },
    SetHostEnvironment {
        name: String,
        environment: Environment,
    },
    ListWorkspaces,
    CreateWorkspace {
        name: String,
        host: String,
        repositories: Vec<Repository>,
        #[serde(default)]
        volumes: Vec<WorkspaceVolume>,
    },
    DeleteWorkspace {
        name: String,
    },
    UpdateWorkspace {
        name: String,
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        repositories: Option<Vec<Repository>>,
        #[serde(default)]
        volumes: Option<Vec<WorkspaceVolume>>,
    },
    ListCodespaces,
    CreateCodespace {
        workspace: String,
        #[serde(default)]
        preset: Option<String>,
        #[serde(default)]
        cpus: Option<u32>,
        #[serde(default)]
        memory_mib: Option<u64>,
        #[serde(default)]
        base_branch: Option<String>,
        #[serde(default)]
        base_branches: BTreeMap<String, String>,
        #[serde(default)]
        client_version: Option<String>,
        #[serde(default)]
        first_message: Option<FirstMessage>,
    },
    CloseCodespace {
        id: String,
    },
    ListGitProviders,
    SetGitProvider(GitProviderInput),
    DeleteGitProvider {
        name: String,
    },
    ListGitCredentials,
    SetGitCredential(GitCredentialInput),
    DeleteGitCredential {
        name: String,
    },
    CheckGitCredential {
        name: String,
    },
    ListPresets,
    SetPreset {
        name: String,
        yaml: String,
        #[serde(default)]
        agents_md: Option<String>,
        #[serde(default)]
        files: Option<Vec<PresetFile>>,
    },
    GetPreset {
        name: String,
    },
    DeletePreset {
        name: String,
    },
    SearchRepositories {
        provider: String,
        #[serde(default)]
        query: String,
    },
    ListBranches {
        provider: String,
        owner: String,
        repository: String,
    },
    ListVolumes,
    CreateVolume(VolumeInput),
    DeleteVolume {
        name: String,
    },
    HostReport {
        report: HostReport,
    },
    HostInstances {
        instances: Vec<InstanceReport>,
    },
    InstanceProgress {
        codespace: String,
        step: String,
    },
    CodespaceBundle,
    CodespaceAgent {
        address: AgentAddress,
    },
    CodespaceProgress {
        step: String,
    },
    CodespaceReady,
    CodespaceFailed {
        error: String,
    },
    GitCredential {
        url: String,
    },
    ListModelAccounts,
    StartModelLogin {
        account: String,
        provider: String,
        method: ModelLoginMethod,
    },
    CompleteModelLogin {
        login: String,
        input: ModelLoginInput,
    },
    CancelModelLogin {
        login: String,
    },
    SetModelApiKey {
        account: String,
        provider: String,
        api_key: Secret,
    },
    DeleteModelAccount {
        name: String,
    },
    ListModels {
        preset: String,
    },
    ListSecrets,
    SetSecret {
        name: String,
        value: Secret,
    },
    DeleteSecret {
        name: String,
    },
    GetWorkspaceNotes {
        workspace: String,
    },
    SetWorkspaceNotes {
        workspace: String,
        text: String,
    },
    ListSessions {
        #[serde(default)]
        codespace: Option<String>,
    },
    ModelCredentials,
    CodespaceNotes {
        text: String,
    },
    SessionSummaries {
        #[serde(default)]
        sessions: Vec<SessionSummary>,
        #[serde(default)]
        removed: Vec<String>,
        #[serde(default)]
        full: bool,
    },
    TakeFirstMessage,
    CodespaceWarnings {
        workspace: String,
        #[serde(default)]
        preset: Option<String>,
    },
    CheckHost {
        name: String,
        #[serde(default)]
        docker: bool,
    },
    SetHostSsh {
        name: String,
        #[serde(default)]
        destination: Option<String>,
    },
    HostVolumes {
        name: String,
    },
    StopCodespace {
        id: String,
    },
    ResumeCodespace {
        id: String,
    },
    SetHostLimits {
        name: String,
        limits: HostLimits,
    },
    CodespaceConnections {
        id: String,
        #[serde(default)]
        types: Vec<String>,
    },
    ListAudit {
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        before: Option<u64>,
    },
    CodespaceActivity {
        active: bool,
        #[serde(default)]
        metrics: Option<CodespaceMetrics>,
    },
    ArchiveEvents {
        codespace: String,
        session: String,
        #[serde(default)]
        after: Option<u64>,
    },
    DeleteArchive {
        codespace: String,
    },
}

pub const DEFAULT_AUDIT_LIMIT: u32 = 100;
pub const MAX_AUDIT_LIMIT: u32 = 1000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello(ServerHello),
    Rejected(OracleError),
    Response(Response),
    Event { event: Box<Event> },
    Call { id: u64, call: HostCall },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: u32,
    pub oracle: OracleInfo,
    pub peer: PeerInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleInfo {
    pub version: String,
    pub protocol: u32,
    pub url: String,
    pub host_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerInfo {
    pub id: u64,
    pub role: Role,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OracleError>,
}

impl Response {
    pub fn ok(id: u64, result: impl Serialize) -> Self {
        match serde_json::to_value(result) {
            Ok(value) => Self {
                id,
                result: Some(value),
                error: None,
            },
            Err(err) => Self::error(id, OracleError::new(ErrorCode::Internal, err.to_string())),
        }
    }

    pub fn error(id: u64, error: OracleError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Device,
    Host,
    Codespace,
    #[serde(other)]
    Unknown,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Device => "device",
            Self::Host => "host",
            Self::Codespace => "codespace",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "device" => Self::Device,
            "host" => Self::Host,
            "codespace" => Self::Codespace,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: u64,
    pub label: String,
    pub added_at: u64,
    #[serde(default)]
    pub last_seen: Option<u64>,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub id: String,
    pub role: Role,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(default)]
    pub created_by: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Devices {
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceResult {
    pub device: Device,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairings {
    pub pairings: Vec<Pairing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingLink {
    pub pairing: Pairing,
    pub link: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: u64,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_label: Option<String>,
    pub action: String,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLog {
    #[serde(default)]
    pub entries: Vec<AuditEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    DeviceChanged {
        device: Device,
    },
    DeviceRemoved {
        id: u64,
    },
    PairingChanged {
        pairing: Pairing,
    },
    PairingRemoved {
        id: String,
        used: bool,
    },
    HostChanged {
        host: Host,
    },
    HostRemoved {
        name: String,
    },
    WorkspaceChanged {
        workspace: Workspace,
    },
    WorkspaceRemoved {
        name: String,
    },
    CodespaceChanged {
        codespace: Codespace,
    },
    CodespaceProgress {
        id: String,
        step: String,
    },
    CodespaceRemoved {
        id: String,
    },
    GitProviderChanged {
        provider: GitProvider,
    },
    GitProviderRemoved {
        name: String,
    },
    GitCredentialChanged {
        credential: GitCredential,
    },
    GitCredentialRemoved {
        name: String,
    },
    PresetChanged {
        preset: Preset,
    },
    PresetRemoved {
        name: String,
    },
    VolumeChanged {
        volume: Volume,
    },
    VolumeRemoved {
        name: String,
    },
    ModelAccountChanged {
        account: ModelAccount,
    },
    ModelAccountRemoved {
        name: String,
    },
    ModelLoginFinished {
        login: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<ModelAccount>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    SecretChanged {
        secret: SecretInfo,
    },
    SecretRemoved {
        name: String,
    },
    WorkspaceNotesChanged {
        workspace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_at: Option<u64>,
    },
    SessionChanged {
        session: CodespaceSession,
    },
    SessionRemoved {
        codespace: String,
        id: String,
    },
    ConfigChanged {
        #[serde(default)]
        presets: Vec<String>,
    },
    DeviceKeysChanged,
    GitChanged,
    EventsLost,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    UnknownCommand,
    NotFound,
    Forbidden,
    Conflict,
    Unavailable,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct OracleError {
    pub code: ErrorCode,
    pub message: String,
}

impl OracleError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(message)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidMessage {
    pub id: Option<u64>,
    pub error: OracleError,
}

pub fn parse_client_message(bytes: &[u8]) -> Result<ClientMessage, InvalidMessage> {
    let invalid = |id: Option<u64>, code: ErrorCode, message: String| InvalidMessage {
        id,
        error: OracleError::new(code, message),
    };
    let value: Value = serde_json::from_slice(bytes).map_err(|err| {
        invalid(
            None,
            ErrorCode::BadRequest,
            format!("malformed JSON: {err}"),
        )
    })?;
    if value.get("type").and_then(Value::as_str) == Some("request") {
        let id = value.get("id").and_then(Value::as_u64);
        let Some(id) = id else {
            return Err(invalid(
                None,
                ErrorCode::BadRequest,
                "a request needs an id".into(),
            ));
        };
        let command = value.get("command").cloned().unwrap_or(Value::Null);
        let name = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        return match serde_json::from_value::<Command>(command) {
            Ok(command) => Ok(ClientMessage::Request(Request { id, command })),
            Err(err) if err.to_string().starts_with("unknown variant") => Err(invalid(
                Some(id),
                ErrorCode::UnknownCommand,
                format!("the oracle does not know the command {name:?}"),
            )),
            Err(err) => Err(invalid(
                Some(id),
                ErrorCode::BadRequest,
                format!("malformed command {name:?}: {err}"),
            )),
        };
    }
    serde_json::from_value(value).map_err(|err| {
        invalid(
            None,
            ErrorCode::BadRequest,
            format!("malformed message: {err}"),
        )
    })
}

pub fn parse_server_message(bytes: &[u8]) -> Result<Option<ServerMessage>, serde_json::Error> {
    let value: Value = serde_json::from_slice(bytes)?;
    let known = matches!(
        value.get("type").and_then(Value::as_str),
        Some("hello" | "rejected" | "response" | "event" | "call")
    );
    if !known {
        return Ok(None);
    }
    serde_json::from_value(value).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_have_a_stable_json_form() {
        let request = ClientMessage::Request(Request {
            id: 3,
            command: Command::RenameDevice {
                id: 7,
                label: "phone".into(),
            },
        });
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "type": "request",
                "id": 3,
                "command": {"type": "rename_device", "id": 7, "label": "phone"},
            })
        );
        let event = ServerMessage::Event {
            event: Box::new(Event::DeviceRemoved { id: 7 }),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type": "event", "event": {"type": "device_removed", "id": 7}})
        );
        let response = ServerMessage::Response(Response::ok(3, Empty {}));
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::json!({"type": "response", "id": 3, "result": {}})
        );
    }

    #[test]
    fn requests_parse_with_unknown_fields() {
        let bytes =
            br#"{"type":"request","id":1,"extra":true,"command":{"type":"list_devices","new":1}}"#;
        assert_eq!(
            parse_client_message(bytes).unwrap(),
            ClientMessage::Request(Request {
                id: 1,
                command: Command::ListDevices,
            })
        );
        let hello = br#"{"type":"hello","version":9,"client":"test","role":"x"}"#;
        assert_eq!(
            parse_client_message(hello).unwrap(),
            ClientMessage::Hello(ClientHello {
                version: 9,
                client: "test".into(),
            })
        );
        let token = br#"{"type":"request","id":2,"command":{"type":"set_push_token"}}"#;
        assert_eq!(
            parse_client_message(token).unwrap(),
            ClientMessage::Request(Request {
                id: 2,
                command: Command::SetPushToken { token: None },
            })
        );
    }

    #[test]
    fn bad_requests_keep_their_id() {
        let unknown = br#"{"type":"request","id":5,"command":{"type":"make_coffee"}}"#;
        let err = parse_client_message(unknown).unwrap_err();
        assert_eq!(err.id, Some(5));
        assert_eq!(err.error.code, ErrorCode::UnknownCommand);
        assert!(err.error.message.contains("make_coffee"), "{err:?}");

        let malformed = br#"{"type":"request","id":6,"command":{"type":"revoke_device"}}"#;
        let err = parse_client_message(malformed).unwrap_err();
        assert_eq!(err.id, Some(6));
        assert_eq!(err.error.code, ErrorCode::BadRequest);

        let err = parse_client_message(b"{").unwrap_err();
        assert_eq!(err.id, None);
        let err = parse_client_message(br#"{"type":"request","command":{}}"#).unwrap_err();
        assert_eq!(err.id, None);
    }

    #[test]
    fn clients_skip_what_they_do_not_know() {
        assert_eq!(
            parse_server_message(br#"{"type":"news","text":"hi"}"#).unwrap(),
            None
        );
        let event = br#"{"type":"event","event":{"type":"backup_changed","id":1}}"#;
        assert_eq!(
            parse_server_message(event).unwrap(),
            Some(ServerMessage::Event {
                event: Box::new(Event::Unknown)
            })
        );
        let device = br#"{"type":"event","event":{"type":"device_changed","device":{"id":1,"label":"a","added_at":2,"color":"red"}}}"#;
        assert_eq!(
            parse_server_message(device).unwrap(),
            Some(ServerMessage::Event {
                event: Box::new(Event::DeviceChanged {
                    device: Device {
                        id: 1,
                        label: "a".into(),
                        added_at: 2,
                        last_seen: None,
                        push: false,
                        connected: false,
                    }
                })
            })
        );
        let error = br#"{"type":"response","id":1,"error":{"code":"too_late","message":"m"}}"#;
        let Some(ServerMessage::Response(response)) = parse_server_message(error).unwrap() else {
            panic!("expected a response");
        };
        assert_eq!(response.error.unwrap().code, ErrorCode::Unknown);
        let hello = br#"{"type":"hello","version":2,"oracle":{"version":"1","protocol":2,"url":"u","host_id":"h"},"peer":{"id":1,"role":"admin","label":"l"}}"#;
        let Some(ServerMessage::Hello(hello)) = parse_server_message(hello).unwrap() else {
            panic!("expected a hello");
        };
        assert_eq!(hello.peer.role, Role::Unknown);
        assert!(parse_server_message(br#"{"type":"hello"}"#).is_err());
    }

    #[test]
    fn hosts_get_calls_and_send_replies() {
        let call = ServerMessage::Call {
            id: 4,
            call: HostCall::DeleteInstance {
                codespace: "c1".into(),
                instance: "loom-c1".into(),
            },
        };
        let bytes = encode(&call).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            serde_json::json!({
                "type": "call",
                "id": 4,
                "call": {"type": "delete_instance", "codespace": "c1", "instance": "loom-c1"},
            })
        );
        assert_eq!(parse_server_message(&bytes).unwrap(), Some(call));
        let reply = br#"{"type":"reply","id":4,"result":{"ok":true},"extra":1}"#;
        assert_eq!(
            parse_client_message(reply).unwrap(),
            ClientMessage::Reply(Reply {
                id: 4,
                result: Some(serde_json::json!({"ok": true})),
                error: None,
            })
        );
    }

    #[test]
    fn roles_round_trip_as_text() {
        for role in [Role::Device, Role::Host, Role::Codespace] {
            assert_eq!(Role::parse(role.as_str()), role);
            assert_eq!(
                serde_json::to_value(role).unwrap(),
                Value::String(role.to_string())
            );
        }
        assert_eq!(Role::parse("admin"), Role::Unknown);
    }
}
