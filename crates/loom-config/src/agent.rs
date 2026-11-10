use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dirs::expand_home;
use crate::error::ConfigError;
use crate::units::{ByteSize, HumanDuration};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub version: u32,
    pub allowed_roots: Vec<PathBuf>,
    pub relay: RelayConfig,
    pub socket: SocketConfig,
    pub storage: StorageConfig,
    pub lifecycle: LifecycleConfig,
    pub instructions: InstructionsConfig,
    pub llm: LlmConfig,
    pub tools: ToolsConfig,
    pub terminal: TerminalConfig,
    pub policy: PolicyConfig,
    pub sandbox: SandboxConfig,
    pub context: ContextConfig,
    pub logging: LoggingConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            allowed_roots: vec![PathBuf::from("~")],
            relay: RelayConfig::default(),
            socket: SocketConfig::default(),
            storage: StorageConfig::default(),
            lifecycle: LifecycleConfig::default(),
            instructions: InstructionsConfig::default(),
            llm: LlmConfig::default(),
            tools: ToolsConfig::default(),
            terminal: TerminalConfig::default(),
            policy: PolicyConfig::default(),
            sandbox: SandboxConfig::default(),
            context: ContextConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RelayConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloudflare_access: Option<CloudflareAccessConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloudflareAccessConfig {
    pub client_id_env: String,
    pub client_secret_env: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<PathBuf>,
    /// How long a terminal survives without a connected client, so a client that
    /// reconnects can reattach to it. `0s` closes terminals on disconnect.
    pub detached_timeout: HumanDuration,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: None,
            detached_timeout: HumanDuration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SocketConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_workspace: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InstructionsConfig {
    pub max_bytes: ByteSize,
}

impl Default for InstructionsConfig {
    fn default() -> Self {
        Self {
            max_bytes: ByteSize::kib(64),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub providers: ProvidersConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ProvidersConfig(pub BTreeMap<String, Option<Value>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSetting<'a> {
    Default,
    Disabled,
    Configured(&'a Value),
}

impl ProvidersConfig {
    pub fn setting(&self, provider: &str) -> ProviderSetting<'_> {
        match self.0.get(provider) {
            None => ProviderSetting::Default,
            Some(None) | Some(Some(Value::Null)) => ProviderSetting::Disabled,
            Some(Some(value)) => ProviderSetting::Configured(value),
        }
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub read: ReadToolConfig,
    pub shell: ShellToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ReadToolConfig {
    pub max_lines: usize,
    pub max_bytes: ByteSize,
}

impl Default for ReadToolConfig {
    fn default() -> Self {
        Self {
            max_lines: 2000,
            max_bytes: ByteSize::kib(50),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShellToolConfig {
    pub program: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<HumanDuration>,
    pub max_lines: usize,
    pub max_bytes: ByteSize,
    pub pty: bool,
}

impl Default for ShellToolConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("/bin/bash"),
            timeout: None,
            max_lines: 2000,
            max_bytes: ByteSize::kib(50),
            pty: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub default: PolicyAction,
    pub rules: Vec<PolicyRule>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default: PolicyAction::Ask,
            rules: vec![PolicyRule {
                tool: "read".into(),
                action: PolicyAction::Allow,
                command_prefix: Vec::new(),
            }],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyRule {
    pub tool: String,
    pub action: PolicyAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_prefix: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Allow,
    #[default]
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub backend: SandboxBackend,
    pub network: bool,
    pub writable: Vec<PathBuf>,
    pub max_output: ByteSize,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: SandboxBackend::None,
            network: true,
            writable: Vec::new(),
            max_output: ByteSize::mib(1),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    #[default]
    None,
    Landlock,
    Bubblewrap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub max_tool_output_tokens: u32,
    pub window_tokens: u64,
    pub compaction_threshold: f64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tool_output_tokens: 8000,
            window_tokens: 200_000,
            compaction_threshold: 0.8,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: LogLevel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl AgentConfig {
    pub fn runtime_dir(&self, dirs: &crate::Dirs) -> PathBuf {
        self.socket
            .dir
            .clone()
            .unwrap_or_else(|| dirs.default_runtime_dir())
    }

    pub fn data_dir(&self, dirs: &crate::Dirs) -> PathBuf {
        self.storage
            .data_dir
            .clone()
            .unwrap_or_else(|| dirs.default_data_dir())
    }

    pub fn expand_paths(&mut self, home: &Path) {
        let expand = |path: &mut PathBuf| *path = expand_home(path, home);
        self.allowed_roots.iter_mut().for_each(expand);
        self.socket.dir.iter_mut().for_each(expand);
        self.storage.data_dir.iter_mut().for_each(expand);
        self.lifecycle.default_workspace.iter_mut().for_each(expand);
        self.terminal.shell.iter_mut().for_each(expand);
        self.sandbox.writable.iter_mut().for_each(expand);
        expand(&mut self.tools.shell.program);
    }

    pub fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: &str, message: String| ConfigError::invalid(origin, field, message);

        if self.version != CONFIG_VERSION {
            return Err(invalid(
                "version",
                format!(
                    "unsupported version {}; expected {CONFIG_VERSION}",
                    self.version
                ),
            ));
        }
        for (index, root) in self.allowed_roots.iter().enumerate() {
            if !root.is_absolute() {
                return Err(invalid(
                    &format!("allowed_roots[{index}]"),
                    format!("{} must be an absolute path", root.display()),
                ));
            }
        }

        if self.relay.enabled {
            match &self.relay.url {
                None => {
                    return Err(invalid(
                        "relay.url",
                        "is required when the relay is enabled".into(),
                    ));
                }
                Some(url) => check_url_scheme(url, &["wss://", "ws://"])
                    .map_err(|message| invalid("relay.url", message))?,
            }
        }
        if let Some(access) = &self.relay.cloudflare_access {
            if access.client_id_env.trim().is_empty() {
                return Err(invalid(
                    "relay.cloudflare_access.client_id_env",
                    "must not be empty".into(),
                ));
            }
            if access.client_secret_env.trim().is_empty() {
                return Err(invalid(
                    "relay.cloudflare_access.client_secret_env",
                    "must not be empty".into(),
                ));
            }
        }

        if self.instructions.max_bytes.as_u64() == 0 {
            return Err(invalid(
                "instructions.max_bytes",
                "must be greater than zero".into(),
            ));
        }

        if let Some(model) = &self.llm.default_model {
            let Some((provider, name)) = model.split_once('/') else {
                return Err(invalid(
                    "llm.default_model",
                    format!("{model:?} must have the form <provider>/<model>"),
                ));
            };
            if provider.is_empty() || name.is_empty() {
                return Err(invalid(
                    "llm.default_model",
                    format!("{model:?} must have the form <provider>/<model>"),
                ));
            }
        }
        for id in self.llm.providers.ids() {
            if id.is_empty() || id.contains('/') {
                return Err(invalid(
                    "llm.providers",
                    format!("{id:?} is not a valid provider ID"),
                ));
            }
        }

        if self.tools.read.max_lines == 0 {
            return Err(invalid(
                "tools.read.max_lines",
                "must be greater than zero".into(),
            ));
        }
        if self.tools.read.max_bytes.as_u64() == 0 {
            return Err(invalid(
                "tools.read.max_bytes",
                "must be greater than zero".into(),
            ));
        }
        if self.tools.shell.program.as_os_str().is_empty() {
            return Err(invalid("tools.shell.program", "must not be empty".into()));
        }
        if self.tools.shell.max_lines == 0 {
            return Err(invalid(
                "tools.shell.max_lines",
                "must be greater than zero".into(),
            ));
        }
        if self.tools.shell.max_bytes.as_u64() == 0 {
            return Err(invalid(
                "tools.shell.max_bytes",
                "must be greater than zero".into(),
            ));
        }
        if self
            .tools
            .shell
            .timeout
            .is_some_and(|timeout| timeout.as_duration().is_zero())
        {
            return Err(invalid(
                "tools.shell.timeout",
                "must be greater than zero".into(),
            ));
        }

        for (index, rule) in self.policy.rules.iter().enumerate() {
            let field = format!("policy.rules[{index}]");
            if rule.tool.trim().is_empty() {
                return Err(invalid(
                    &format!("{field}.tool"),
                    "must not be empty".into(),
                ));
            }
            if !rule.command_prefix.is_empty() && rule.tool != "shell" {
                return Err(invalid(
                    &format!("{field}.command_prefix"),
                    "is only allowed for the shell tool".into(),
                ));
            }
            if rule
                .command_prefix
                .iter()
                .any(|prefix| prefix.trim().is_empty())
            {
                return Err(invalid(
                    &format!("{field}.command_prefix"),
                    "must not contain empty prefixes".into(),
                ));
            }
        }

        if self.sandbox.max_output.as_u64() == 0 {
            return Err(invalid(
                "sandbox.max_output",
                "must be greater than zero".into(),
            ));
        }
        if self.context.max_tool_output_tokens == 0 {
            return Err(invalid(
                "context.max_tool_output_tokens",
                "must be greater than zero".into(),
            ));
        }
        if self.context.window_tokens < 1000 {
            return Err(invalid(
                "context.window_tokens",
                "must be at least 1000".into(),
            ));
        }
        let threshold = self.context.compaction_threshold;
        if !(threshold > 0.0 && threshold <= 1.0) {
            return Err(invalid(
                "context.compaction_threshold",
                format!("{threshold} must be greater than 0 and at most 1"),
            ));
        }
        Ok(())
    }
}

fn check_url_scheme(url: &str, schemes: &[&str]) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    match schemes.iter().find(|scheme| lower.starts_with(**scheme)) {
        Some(scheme) if url.len() > scheme.len() => Ok(()),
        _ => Err(format!("{url:?} must start with {}", schemes.join(" or "))),
    }
}
