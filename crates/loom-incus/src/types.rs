use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Server {
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub api_extensions: Vec<String>,
    #[serde(default)]
    pub environment: ServerEnvironment,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ServerEnvironment {
    #[serde(default)]
    pub server_version: String,
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub kernel_architecture: String,
    #[serde(default)]
    pub kernel_version: String,
    #[serde(default)]
    pub storage: String,
    #[serde(default)]
    pub storage_version: String,
    #[serde(default)]
    pub project: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Resources {
    #[serde(default)]
    pub cpu: CpuResources,
    #[serde(default)]
    pub memory: MemoryResources,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CpuResources {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct MemoryResources {
    #[serde(default)]
    pub total: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    #[serde(default)]
    pub devices: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub profiles: Vec<String>,
}

impl Instance {
    pub fn running(&self) -> bool {
        self.status == "Running"
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct InstanceState {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub network: Option<BTreeMap<String, NetworkState>>,
}

impl InstanceState {
    pub fn global_ipv4(&self) -> Option<String> {
        self.network
            .as_ref()?
            .iter()
            .filter(|(name, _)| name.as_str() != "lo")
            .flat_map(|(_, state)| &state.addresses)
            .find(|address| address.family == "inet" && address.scope == "global")
            .map(|address| address.address.clone())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NetworkState {
    #[serde(default)]
    pub addresses: Vec<NetworkAddress>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NetworkAddress {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAlias {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub fingerprint: String,
    #[serde(default)]
    pub aliases: Vec<ImageAlias>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StoragePool {
    pub name: String,
    #[serde(default)]
    pub driver: String,
    #[serde(default)]
    pub status: String,
    pub config: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StorageVolume {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    #[serde(default)]
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StorageVolumeUsage {
    #[serde(default)]
    pub used: u64,
    #[serde(default)]
    pub total: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StorageVolumeState {
    #[serde(default)]
    pub usage: Option<StorageVolumeUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Network {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub managed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NetworkAcl {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub egress: Vec<serde_json::Value>,
    #[serde(default)]
    pub ingress: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Operation {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub status_code: i64,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub err: String,
}

impl Operation {
    pub fn running(&self) -> bool {
        matches!(self.status_code, 100..=199) || self.status == "Running"
    }

    pub fn succeeded(&self) -> bool {
        self.status_code == 200 || self.status == "Success"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAction {
    Start,
    Stop,
    Restart,
}

impl StateAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOwner {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

impl FileOwner {
    pub const ROOT: Self = Self {
        uid: 0,
        gid: 0,
        mode: 0o644,
    };

    pub fn root(mode: u32) -> Self {
        Self { mode, ..Self::ROOT }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exec {
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub user: u32,
    pub group: u32,
    pub cwd: Option<String>,
}

impl Exec {
    pub fn new<S: Into<String>>(command: impl IntoIterator<Item = S>) -> Self {
        Self {
            command: command.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }

    pub fn stderr_tail(&self, lines: usize) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let text = if text.trim().is_empty() {
            String::from_utf8_lossy(&self.stdout)
        } else {
            text
        };
        let all: Vec<&str> = text.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_has_the_global_ipv4_address() {
        let state: InstanceState = serde_json::from_value(serde_json::json!({
            "status": "Running",
            "network": {
                "lo": {"addresses": [{"family": "inet", "address": "127.0.0.1", "scope": "local"}]},
                "eth0": {"addresses": [
                    {"family": "inet6", "address": "fd42::1", "scope": "global"},
                    {"family": "inet", "address": "10.0.0.5", "scope": "global"}
                ]}
            }
        }))
        .unwrap();
        assert_eq!(state.global_ipv4().as_deref(), Some("10.0.0.5"));
        assert_eq!(InstanceState::default().global_ipv4(), None);
    }

    #[test]
    fn the_output_shows_the_last_lines() {
        let output = ExecOutput {
            code: 1,
            stdout: b"a\nb\n".to_vec(),
            stderr: b"one\ntwo\nthree\n".to_vec(),
        };
        assert_eq!(output.stderr_tail(2), "two\nthree");
        let quiet = ExecOutput {
            stderr: Vec::new(),
            ..output
        };
        assert_eq!(quiet.stderr_tail(5), "a\nb");
    }
}
