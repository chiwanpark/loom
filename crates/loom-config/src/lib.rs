mod agent;
mod dirs;
mod error;
mod load;
mod units;

pub use agent::{
    AgentConfig, CONFIG_VERSION, CloudflareAccessConfig, ContextConfig, InstructionsConfig,
    LifecycleConfig, LlmConfig, LogLevel, LoggingConfig, PolicyAction, PolicyConfig, PolicyRule,
    ProviderSetting, ProvidersConfig, ReadToolConfig, RelayConfig, SandboxBackend, SandboxConfig,
    ShellToolConfig, SocketConfig, StorageConfig, TerminalConfig, ToolsConfig,
};
pub use dirs::{AGENT_CONFIG_FILE, AGENTS_MD, APP_DIR, CONFIG_ENV, Dirs, expand_home};
pub use error::ConfigError;
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
