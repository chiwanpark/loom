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
    pub telemetry: TelemetryConfig,
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
            telemetry: TelemetryConfig::default(),
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
    /// Replaces Loom's built-in instructions at the start of the system prompt.
    /// The `AGENTS.md` files and the runtime section are still appended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

impl Default for InstructionsConfig {
    fn default() -> Self {
        Self {
            max_bytes: ByteSize::kib(64),
            system_prompt: None,
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
    pub web_search: WebSearchToolConfig,
    pub thread: ThreadToolConfig,
}

/// The operation thread: a text board each session keeps for the agent's
/// notes on its work, which the agent and the user both read and edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadToolConfig {
    /// The largest thread, in bytes, that the agent or the user may save.
    pub max_bytes: ByteSize,
}

impl Default for ThreadToolConfig {
    fn default() -> Self {
        Self {
            max_bytes: ByteSize::kib(64),
        }
    }
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

/// The `search_web` tool, which asks an OpenAI model to search the web. It
/// signs in with the `openai-codex` credentials and is blocked without them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebSearchToolConfig {
    /// The model that searches.
    pub model: String,
    /// How many sources a search returns when the model doesn't say.
    pub max_sources: usize,
    /// The most sources the model may ask for.
    pub max_allowed_sources: usize,
    pub timeout: HumanDuration,
    /// The Responses endpoint of the ChatGPT backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// How hard the searching model reasons. Unset leaves it to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<SearchReasoningEffort>,
}

impl Default for WebSearchToolConfig {
    fn default() -> Self {
        Self {
            model: "gpt-5.6-luna".into(),
            max_sources: 5,
            max_allowed_sources: 10,
            timeout: HumanDuration::from_secs(120),
            endpoint: None,
            reasoning_effort: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
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

/// The context window when neither the config nor the provider gives one.
pub const DEFAULT_WINDOW_TOKENS: u64 = 200_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub max_tool_output_tokens: u32,
    /// The context window to compact against. Unset, it is the window the
    /// provider reports for the model, or [`DEFAULT_WINDOW_TOKENS`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_tokens: Option<u64>,
    pub compaction_threshold: f64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tool_output_tokens: 8000,
            window_tokens: None,
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

/// OpenTelemetry export of session metrics and events over OTLP.
///
/// Unset connection settings fall back to the standard `OTEL_EXPORTER_OTLP_*`
/// environment variables, and `OTEL_RESOURCE_ATTRIBUTES` is honoured. Changes
/// take effect when the agent restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub enabled: bool,
    /// `http/protobuf` unless `OTEL_EXPORTER_OTLP_PROTOCOL` says otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<OtlpProtocol>,
    /// The collector's base URL, such as `http://localhost:4318`. Over HTTP,
    /// `/v1/metrics` and `/v1/logs` are appended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Extra request headers, for example an `authorization` token.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    pub metrics: bool,
    pub logs: bool,
    pub metrics_interval: HumanDuration,
    pub logs_interval: HumanDuration,
    pub temporality: Temporality,
    /// The prefix of metric and event names. `claude_code` makes the data
    /// match Claude Code's, so its dashboards work unchanged.
    pub prefix: String,
    pub service_name: String,
    /// Added to the resource of every metric and event.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub resource_attributes: BTreeMap<String, String>,
    /// Include the prompt text in `user_prompt` events instead of `<REDACTED>`.
    pub log_user_prompts: bool,
    /// Include the response text in `assistant_response` events.
    pub log_assistant_responses: bool,
    /// Include tool arguments, commands and error messages in tool events.
    pub log_tool_details: bool,
    /// The longest text attribute, in characters, before it is truncated.
    pub content_max_length: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            protocol: None,
            endpoint: None,
            headers: BTreeMap::new(),
            metrics: true,
            logs: true,
            metrics_interval: HumanDuration::from_secs(60),
            logs_interval: HumanDuration::from_secs(5),
            temporality: Temporality::default(),
            prefix: "loom".into(),
            service_name: "loom-agent".into(),
            resource_attributes: BTreeMap::new(),
            log_user_prompts: false,
            log_assistant_responses: false,
            log_tool_details: false,
            content_max_length: 61_440,
        }
    }
}

impl TelemetryConfig {
    pub fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: &str, message: String| ConfigError::invalid(origin, field, message);
        if self
            .endpoint
            .as_deref()
            .is_some_and(|url| check_url_scheme(url, &["http://", "https://"]).is_err())
        {
            return Err(invalid(
                "telemetry.endpoint",
                "must start with http:// or https://".into(),
            ));
        }
        for (field, interval) in [
            ("telemetry.metrics_interval", self.metrics_interval),
            ("telemetry.logs_interval", self.logs_interval),
        ] {
            if interval.as_duration().is_zero() {
                return Err(invalid(field, "must be greater than zero".into()));
            }
        }
        if self.prefix.trim().is_empty() {
            return Err(invalid("telemetry.prefix", "must not be empty".into()));
        }
        if self.service_name.trim().is_empty() {
            return Err(invalid(
                "telemetry.service_name",
                "must not be empty".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum OtlpProtocol {
    #[serde(rename = "grpc")]
    Grpc,
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
    #[serde(rename = "http/json")]
    HttpJson,
}

impl std::str::FromStr for OtlpProtocol {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "grpc" => Ok(Self::Grpc),
            "http/protobuf" => Ok(Self::HttpProtobuf),
            "http/json" => Ok(Self::HttpJson),
            other => Err(format!(
                "unknown OTLP protocol {other:?}; expected grpc, http/protobuf or http/json"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Temporality {
    #[default]
    Cumulative,
    Delta,
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

        self.telemetry.validate(origin)?;
        self.tools.web_search.validate(origin)?;

        if self.instructions.max_bytes.as_u64() == 0 {
            return Err(invalid(
                "instructions.max_bytes",
                "must be greater than zero".into(),
            ));
        }
        if self
            .instructions
            .system_prompt
            .as_deref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err(invalid(
                "instructions.system_prompt",
                "must not be empty; remove the key to use the built-in instructions".into(),
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
        if self.tools.thread.max_bytes.as_u64() == 0 {
            return Err(invalid(
                "tools.thread.max_bytes",
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
        if self.context.window_tokens.is_some_and(|n| n < 1000) {
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

impl WebSearchToolConfig {
    fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: &str, message: &str| {
            ConfigError::invalid(origin, format!("tools.web_search.{field}"), message)
        };
        if self.model.trim().is_empty() {
            return Err(invalid("model", "must not be empty"));
        }
        if self.max_sources == 0 {
            return Err(invalid("max_sources", "must be greater than zero"));
        }
        if self.max_allowed_sources == 0 {
            return Err(invalid("max_allowed_sources", "must be greater than zero"));
        }
        if self.timeout.as_duration().is_zero() {
            return Err(invalid("timeout", "must be greater than zero"));
        }
        if let Some(url) = &self.endpoint {
            check_url_scheme(url, &["http://", "https://"])
                .map_err(|message| invalid("endpoint", &message))?;
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
