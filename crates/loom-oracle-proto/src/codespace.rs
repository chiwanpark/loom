use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    CheckResult, GitSetup, InstanceVolume, PresetBundle, ProviderRequest, WorkspaceCheck,
    WorkspaceVolume,
};

pub const DEFAULT_CPUS: u32 = 2;
pub const DEFAULT_MEMORY_MIB: u64 = 4096;
pub const CONNECTION_RELAY: &str = "relay";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registry_mirrors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_script: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ca_certificates: Vec<String>,
    #[serde(default)]
    pub docker: DockerSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReport {
    pub version: String,
    pub driver: String,
    pub cpus: u32,
    pub memory_mib: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incus_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_codespaces: Option<u32>,
    #[serde(default)]
    pub connection_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<HostCheck>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCheckStatus {
    Ok,
    Warning,
    Failed,
    #[serde(other)]
    Unknown,
}

impl HostCheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for HostCheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCheck {
    pub name: String,
    pub status: HostCheckStatus,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostChecks {
    #[serde(default)]
    pub checks: Vec<HostCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsage {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsages {
    #[serde(default)]
    pub volumes: Vec<VolumeUsage>,
}

pub const MAX_SSH_DESTINATION: usize = 200;

pub fn valid_ssh_destination(destination: &str) -> bool {
    !destination.is_empty()
        && destination.len() <= MAX_SSH_DESTINATION
        && !destination.starts_with('-')
        && !destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\'' | '"' | '`' | '\\'))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub id: u64,
    pub name: String,
    pub added_at: u64,
    #[serde(default)]
    pub connected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<HostReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<u64>,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub codespaces: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reach: Vec<CheckResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<HostCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    pub host: String,
    pub repositories: Vec<Repository>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<WorkspaceVolume>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodespaceState {
    Provisioning,
    Starting,
    Ready,
    Closing,
    Closed,
    Failed,
    Lost,
    #[serde(other)]
    Unknown,
}

impl CodespaceState {
    pub const ALL: [Self; 7] = [
        Self::Provisioning,
        Self::Starting,
        Self::Ready,
        Self::Closing,
        Self::Closed,
        Self::Failed,
        Self::Lost,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Closing => "closing",
            Self::Closed => "closed",
            Self::Failed => "failed",
            Self::Lost => "lost",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == text)
            .unwrap_or(Self::Unknown)
    }

    pub fn is_open(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

impl fmt::Display for CodespaceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAddress {
    pub host_id: String,
    pub host_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Codespace {
    pub id: String,
    pub workspace: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    pub state: CodespaceState,
    pub cpus: u32,
    pub memory_mib: u64,
    pub instance: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<Repository>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentAddress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub first_message: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceKey {
    pub label: String,
    pub pubkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub codespace: String,
    pub workspace: String,
    pub branch: String,
    pub repositories: Vec<Repository>,
    pub devices: Vec<DeviceKey>,
    pub relay_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default)]
    pub git: GitSetup,
    #[serde(default)]
    pub agent: PresetBundle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub link: String,
    pub codespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces_root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateInstance {
    pub codespace: String,
    pub instance: String,
    pub cpus: u32,
    pub memory_mib: u64,
    #[serde(default)]
    pub environment: Environment,
    pub bootstrap: Bootstrap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<InstanceVolume>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceReport {
    pub codespace: String,
    pub instance: String,
    pub oracle: String,
    #[serde(default)]
    pub running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostCall {
    CreateInstance(Box<CreateInstance>),
    DeleteInstance {
        codespace: String,
        instance: String,
    },
    ListInstances,
    CheckWorkspace(WorkspaceCheck),
    ProviderRequest(Box<ProviderRequest>),
    DeleteVolume {
        volume: String,
    },
    Check {
        #[serde(default)]
        docker: bool,
    },
    VolumeUsage {
        volumes: Vec<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hosts {
    pub hosts: Vec<Host>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostResult {
    pub host: Host,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspaces {
    pub workspaces: Vec<Workspace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceResult {
    pub workspace: Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Codespaces {
    pub codespaces: Vec<Codespace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodespaceResult {
    pub codespace: Codespace,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warnings {
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instances {
    pub instances: Vec<InstanceReport>,
}

pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 40
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.ends_with('-')
        && !name.contains("--")
}

pub fn repository_directory(url: &str) -> Option<String> {
    let path = url.trim_end_matches('/').trim_end_matches(".git");
    let name = path.rsplit(['/', ':']).next()?;
    valid_directory(name).then(|| name.to_owned())
}

pub fn valid_directory(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_short_lowercase_words() {
        for name in ["a", "web-app", "acme2", "x-1-y"] {
            assert!(valid_name(name), "{name}");
        }
        for name in ["", "Web", "1st", "a_b", "a-", "a--b", &"a".repeat(41)] {
            assert!(!valid_name(name), "{name}");
        }
    }

    #[test]
    fn ssh_destinations_are_one_safe_word() {
        for ok in ["host", "dev@host.example.com", "ssh://dev@10.0.0.2:2222"] {
            assert!(valid_ssh_destination(ok), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "dev@host extra",
            "a\"b",
            &"h".repeat(201),
        ] {
            assert!(!valid_ssh_destination(bad), "{bad}");
        }
    }

    #[test]
    fn the_directory_comes_from_the_url() {
        assert_eq!(
            repository_directory("https://github.com/acme/server.git").as_deref(),
            Some("server")
        );
        assert_eq!(
            repository_directory("https://github.com/acme/web.app/").as_deref(),
            Some("web.app")
        );
        assert_eq!(
            repository_directory("git@github.com:acme/tool").as_deref(),
            Some("tool")
        );
        assert_eq!(repository_directory("https://github.com/.."), None);
    }

    #[test]
    fn states_round_trip_and_tolerate_new_ones() {
        for state in CodespaceState::ALL {
            assert_eq!(CodespaceState::parse(state.as_str()), state);
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::Value::String(state.to_string())
            );
        }
        assert_eq!(
            serde_json::from_str::<CodespaceState>("\"stopped\"").unwrap(),
            CodespaceState::Unknown
        );
    }

    #[test]
    fn hosts_skip_calls_that_they_do_not_know() {
        let call: HostCall =
            serde_json::from_str(r#"{"type":"stop_instance","name":"x"}"#).unwrap();
        assert_eq!(call, HostCall::Unknown);
        let call: HostCall = serde_json::from_str(
            r#"{"type":"delete_instance","codespace":"c","instance":"loom-c","force":true}"#,
        )
        .unwrap();
        assert_eq!(
            call,
            HostCall::DeleteInstance {
                codespace: "c".into(),
                instance: "loom-c".into()
            }
        );
    }
}
