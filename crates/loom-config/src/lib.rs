mod agent;
mod claude_code;
mod client;
mod dirs;
mod error;
mod host;
mod load;
mod oracle;
mod relay_server;
mod units;

pub use agent::{
    AgentConfig, CONFIG_VERSION, CloudflareAccessConfig, CommentGuardConfig, CommentGuardMode,
    ContextConfig, DEFAULT_WINDOW_TOKENS, EnvironmentConfig, GitConfig, HarmfulConfig,
    InheritEnvironment, InstructionsConfig, LifecycleConfig, LimitsConfig, LlmConfig, LogLevel,
    LoggingConfig, NotesToolConfig, OtlpProtocol, PolicyAction, PolicyConfig, PolicyRule,
    ProviderSetting, ProvidersConfig, ReadToolConfig, ReasoningEffort, RedactionConfig,
    RelayConfig, SandboxBackend, SandboxConfig, ShellToolConfig, SkillsConfig, SocketConfig,
    StorageConfig, TasksToolConfig, TelemetryConfig, Temporality, TerminalConfig, ToolsConfig,
    WebSearchToolConfig, WorktreesConfig, valid_variable_name,
};
pub use claude_code::{ImportedTelemetry, telemetry_from_claude_code};
pub use client::{AppearanceConfig, CLIENT_CONFIG_FILE, ClientConfig, HostEntry, Theme};
pub use dirs::{AGENT_CONFIG_FILE, AGENTS_MD, APP_DIR, CONFIG_ENV, Dirs, expand_home};
pub use error::ConfigError;
pub use host::{
    DEFAULT_INCUS_NETWORK, DEFAULT_INCUS_POOL, DEFAULT_INCUS_PROJECT, HOST_CONFIG_ENV,
    HOST_CONFIG_FILE, HOST_DATA_DIR, HostAgentConfig, HostConfig, HostDriver, HostRules, HostUser,
    IncusCertificates, IncusDriverConfig, ProcessDriverConfig, host_config_schema,
};
pub use load::{MANAGED_SECTIONS, ManagedSettings, strip_managed_sections};
pub use oracle::{
    CodespaceLifecycle, ORACLE_CONFIG_ENV, ORACLE_CONFIG_FILE, ORACLE_DATA_DIR,
    ORACLE_MASTER_KEY_FILE, OracleConfig, oracle_config_schema,
};
pub use relay_server::{
    AccessVerifyConfig, FCM_API_URL, FcmConfig, PushConfig, RELAY_CONFIG_ENV, RELAY_CONFIG_FILE,
    RelayLimits, RelayServerConfig, TlsConfig, relay_config_schema,
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
