mod agent;
mod claude_code;
mod client;
mod dirs;
mod error;
mod load;
mod relay_server;
mod units;

pub use agent::{
    AgentConfig, CONFIG_VERSION, CloudflareAccessConfig, ContextConfig, DEFAULT_WINDOW_TOKENS,
    InstructionsConfig, LifecycleConfig, LimitsConfig, LlmConfig, LogLevel, LoggingConfig,
    NotesToolConfig, OtlpProtocol, PolicyAction, PolicyConfig, PolicyRule, ProviderSetting,
    ProvidersConfig, ReadToolConfig, RelayConfig, SandboxBackend, SandboxConfig,
    SearchReasoningEffort, ShellToolConfig, SocketConfig, StorageConfig, TasksToolConfig,
    TelemetryConfig, Temporality, TerminalConfig, ToolsConfig, WebSearchToolConfig,
};
pub use claude_code::{ImportedTelemetry, telemetry_from_claude_code};
pub use client::{AppearanceConfig, CLIENT_CONFIG_FILE, ClientConfig, HostEntry, Theme};
pub use dirs::{AGENT_CONFIG_FILE, AGENTS_MD, APP_DIR, CONFIG_ENV, Dirs, expand_home};
pub use error::ConfigError;
pub use relay_server::{
    AccessVerifyConfig, RELAY_CONFIG_ENV, RELAY_CONFIG_FILE, RelayLimits, RelayServerConfig,
    TlsConfig, relay_config_schema,
};
pub use units::{ByteSize, HumanDuration};

pub fn agent_config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(AgentConfig)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_describes_top_level_sections() {
        let schema = agent_config_schema();
        let properties = schema["properties"].as_object().unwrap();
        for section in ["relay", "llm", "tools", "policy", "sandbox"] {
            assert!(properties.contains_key(section), "missing {section}");
        }
        assert!(!properties.contains_key("workspaces"));
        assert_eq!(schema["additionalProperties"], false);
    }
}
