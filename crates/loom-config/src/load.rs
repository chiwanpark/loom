use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent::{AgentConfig, PolicyAction};
use crate::dirs::Dirs;
use crate::error::ConfigError;

pub const MANAGED_SECTIONS: &[&str] = &[
    "allowed_roots",
    "socket",
    "storage",
    "lifecycle",
    "worktrees",
    "relay.enabled",
    "relay.url",
    "environment.env_file",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSettings {
    pub relay_url: String,
    pub allowed_roots: Vec<PathBuf>,
    pub workspace: PathBuf,
    pub env_file: PathBuf,
}

fn has_key(document: &Value, key: &str) -> bool {
    let mut value = document;
    for part in key.split('.') {
        match value.get(part) {
            Some(next) => value = next,
            None => return false,
        }
    }
    true
}

fn remove_key(document: &mut Value, key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, parents)) = parts.split_last() else {
        return false;
    };
    let mut value = &mut *document;
    for part in parents {
        match value.get_mut(*part) {
            Some(next) => value = next,
            None => return false,
        }
    }
    let removed = value
        .as_object_mut()
        .is_some_and(|map| map.remove(*last).is_some());
    if removed && let Some(parent) = parents.first() {
        let empty = document
            .get(*parent)
            .and_then(Value::as_object)
            .is_some_and(|map| map.is_empty());
        if empty && let Some(map) = document.as_object_mut() {
            map.remove(*parent);
        }
    }
    removed
}

pub fn strip_managed_sections(text: &str) -> Result<(String, Vec<&'static str>), ConfigError> {
    let mut document: Value = serde_saphyr::from_str(text).map_err(|err| ConfigError::Parse {
        origin: "agent.yaml".to_string(),
        message: err.to_string(),
    })?;
    let removed: Vec<&'static str> = MANAGED_SECTIONS
        .iter()
        .copied()
        .filter(|key| remove_key(&mut document, key))
        .collect();
    if removed.is_empty() {
        return Ok((text.to_owned(), removed));
    }
    let yaml = serde_saphyr::to_string(&document).map_err(|err| ConfigError::Parse {
        origin: "agent.yaml".to_string(),
        message: err.to_string(),
    })?;
    Ok((yaml, removed))
}

pub(crate) fn parse_yaml<T: serde::de::DeserializeOwned + Default>(
    text: &str,
    origin: &str,
) -> Result<T, ConfigError> {
    let parse_error = |err: serde_saphyr::Error| ConfigError::Parse {
        origin: origin.to_string(),
        message: err.to_string(),
    };
    let document: Value = serde_saphyr::from_str(text).map_err(parse_error)?;
    if document.is_null() {
        Ok(T::default())
    } else {
        serde_saphyr::from_str::<T>(text).map_err(parse_error)
    }
}

impl AgentConfig {
    pub fn load(path: &Path, dirs: &Dirs) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &path.display().to_string(), dirs),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut config = Self::default();
                config.expand_paths(&dirs.home);
                Ok(config)
            }
            Err(source) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn parse(text: &str, origin: &str, dirs: &Dirs) -> Result<Self, ConfigError> {
        let mut config: Self = parse_yaml(text, origin)?;
        config.expand_paths(&dirs.home);
        config.validate(origin)?;
        for (key, used) in [
            ("llm.accounts", !config.llm.accounts.is_empty()),
            (
                "environment.secrets",
                !config.environment.secrets.is_empty(),
            ),
        ] {
            if used {
                return Err(ConfigError::invalid(
                    origin,
                    key,
                    "only a preset of the oracle can name credentials",
                ));
            }
        }
        Ok(config)
    }

    pub fn parse_preset(text: &str, origin: &str, dirs: &Dirs) -> Result<Self, ConfigError> {
        let document: Value = serde_saphyr::from_str(text).map_err(|err| ConfigError::Parse {
            origin: origin.to_string(),
            message: err.to_string(),
        })?;
        let mut config: Self = parse_yaml(text, origin)?;
        if let Some(key) = MANAGED_SECTIONS.iter().find(|key| has_key(&document, key)) {
            return Err(ConfigError::invalid(
                origin,
                *key,
                "the managed mode sets this itself; remove it",
            ));
        }
        if !has_key(&document, "policy.default") {
            config.policy.default = PolicyAction::Allow;
        }
        config.expand_paths(&dirs.home);
        config.validate(origin)?;
        Ok(config)
    }

    pub fn read_preset(path: &Path, dirs: &Dirs) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        Self::parse_preset(&text, &path.display().to_string(), dirs)
    }

    pub fn apply_managed(&mut self, settings: &ManagedSettings) {
        self.relay.enabled = true;
        self.relay.url = Some(settings.relay_url.clone());
        self.allowed_roots = settings.allowed_roots.clone();
        self.lifecycle.default_workspace = Some(settings.workspace.clone());
        self.environment.env_file = Some(settings.env_file.clone());
    }

    pub fn load_preset(
        path: &Path,
        dirs: &Dirs,
        settings: &ManagedSettings,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::read_preset(path, dirs)?;
        config.apply_managed(settings);
        config.validate(&path.display().to_string())?;
        Ok(config)
    }

    pub fn to_yaml(&self) -> String {
        serde_saphyr::to_string(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::agent::{LogLevel, ProviderSetting};
    use crate::units::ByteSize;

    const EXAMPLE: &str = include_str!("../tests/fixtures/agent.yaml");

    fn dirs() -> Dirs {
        Dirs {
            home: PathBuf::from("/home/u"),
            config_home: PathBuf::from("/home/u/.config"),
            data_home: PathBuf::from("/home/u/.local/share"),
            state_home: PathBuf::from("/home/u/.local/state"),
            runtime_dir: None,
        }
    }

    fn parse(text: &str) -> Result<AgentConfig, ConfigError> {
        AgentConfig::parse(text, "agent.yaml", &dirs())
    }

    #[test]
    fn managed_sections_leave_an_agent_file_for_a_preset() {
        let (yaml, removed) = strip_managed_sections(EXAMPLE).unwrap();
        assert!(removed.contains(&"allowed_roots"), "{removed:?}");
        AgentConfig::parse_preset(&yaml, "preset", &dirs()).unwrap();
        let text = "relay:\n  enabled: true\n  url: wss://x\nlifecycle:\n  idle: 1h\nllm:\n  default_model: a/b\n";
        let (yaml, removed) = strip_managed_sections(text).unwrap();
        assert_eq!(removed, ["lifecycle", "relay.enabled", "relay.url"]);
        let document: Value = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(
            document,
            serde_json::json!({"llm": {"default_model": "a/b"}})
        );
        let plain = "# mine\nllm:\n  default_model: a/b\n";
        assert_eq!(
            strip_managed_sections(plain).unwrap(),
            (plain.to_owned(), vec![])
        );
    }

    #[test]
    fn parses_documented_example() {
        let config = parse(EXAMPLE).unwrap();
        assert_eq!(
            config.allowed_roots,
            [PathBuf::from("/home/u/src"), PathBuf::from("/home/u/work")]
        );
        assert_eq!(
            config.storage.data_dir,
            Some(PathBuf::from("/home/u/.local/share/loom"))
        );
        assert_eq!(
            config
                .tools
                .shell
                .timeout
                .map(|timeout| timeout.as_duration()),
            Some(Duration::from_secs(600))
        );
        assert_eq!(config.tools.read.max_bytes, ByteSize::kib(50));
        assert_eq!(
            config.tools.allowed_dirs,
            [PathBuf::from("/home/u/notes"), PathBuf::from("/srv/shared")]
        );
        assert_eq!(config.policy.rules.len(), 3);
        assert_eq!(config.policy.rules[1].command_prefix.len(), 4);
        assert_eq!(config.llm.providers.0.len(), 3);
        assert!(config.llm.default_model.is_some());
        assert_eq!(
            config.llm.default_reasoning_effort,
            crate::ReasoningEffort::High
        );
        assert_eq!(
            config.skills.paths,
            [PathBuf::from("/home/u/shared-skills")]
        );
        assert_eq!(config.skills.disabled, ["slow-review"]);
        let environment = &config.environment;
        assert_eq!(environment.inherit, crate::InheritEnvironment::Core);
        assert_eq!(environment.exclude, ["AWS_*"]);
        assert_eq!(environment.set["RUST_BACKTRACE"], "1");
        assert_eq!(environment.env_file, Some(PathBuf::from(".env")));
        assert!(environment.redaction.enabled);
        assert_eq!(
            environment.redaction.allow,
            ["AWS_REGION", "RUST_BACKTRACE"]
        );
        assert_eq!(
            config.worktrees.dir,
            Some(PathBuf::from("/home/u/.config/loom/worktrees"))
        );
    }

    #[test]
    fn env_file_can_be_turned_off() {
        assert_eq!(
            parse("environment:\n  env_file: null\n")
                .unwrap()
                .environment
                .env_file,
            None
        );
    }

    #[test]
    fn empty_file_uses_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.allowed_roots, [PathBuf::from("/home/u")]);
        assert!(config.llm.providers.0.is_empty());
        assert_eq!(
            config.llm.default_reasoning_effort,
            crate::ReasoningEffort::Medium
        );
        assert_eq!(
            config.llm.providers.setting("any"),
            ProviderSetting::Default
        );
    }

    #[test]
    fn missing_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = AgentConfig::load(&dir.path().join("agent.yaml"), &dirs()).unwrap();
        assert_eq!(config.allowed_roots, [PathBuf::from("/home/u")]);
    }

    #[test]
    fn loads_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "logging:\n  level: debug\n").unwrap();
        let config = AgentConfig::load(&path, &dirs()).unwrap();
        assert_eq!(config.logging.level, LogLevel::Debug);
    }

    #[test]
    fn reports_unknown_fields_with_location() {
        let err = parse("tools:\n  shell:\n    progam: /bin/sh\n").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("line 3"), "{message}");
        assert!(message.contains("progam"), "{message}");
    }

    #[test]
    fn parses_custom_system_prompt() {
        let config =
            parse("instructions:\n  system_prompt: |\n    Line one.\n    Line two.\n").unwrap();
        assert_eq!(
            config.instructions.system_prompt.as_deref(),
            Some("Line one.\nLine two.\n")
        );
        assert_eq!(parse("").unwrap().instructions.system_prompt, None);
    }

    #[test]
    fn parses_turn_limits() {
        assert_eq!(parse("").unwrap().limits.max_rounds_per_turn, 200);
        let config = parse("limits:\n  max_rounds_per_turn: 0\n").unwrap();
        assert_eq!(config.limits.max_rounds_per_turn, 0);
        let config = parse("limits:\n  max_rounds_per_turn: 1000\n").unwrap();
        assert_eq!(config.limits.max_rounds_per_turn, 1000);
    }

    #[test]
    fn rejects_removed_workspace_section() {
        let err = parse("workspaces:\n- path: ~/x\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn disables_provider_with_null() {
        let config =
            parse("llm:\n  providers:\n    first: null\n    second:\n      key: value\n").unwrap();
        let providers = &config.llm.providers;
        assert_eq!(providers.setting("first"), ProviderSetting::Disabled);
        assert_eq!(
            providers.setting("second"),
            ProviderSetting::Configured(&serde_json::json!({"key": "value"}))
        );
        assert_eq!(providers.setting("third"), ProviderSetting::Default);
    }

    #[test]
    fn parses_telemetry() {
        let config = parse(
            "telemetry:\n  enabled: true\n  protocol: grpc\n  endpoint: http://collector:4317\n  \
             headers:\n    authorization: Bearer x\n  metrics_interval: 10s\n  \
             temporality: delta\n  prefix: claude_code\n",
        )
        .unwrap();
        let telemetry = config.telemetry;
        assert!(telemetry.enabled && telemetry.metrics && telemetry.logs);
        assert_eq!(telemetry.protocol, Some(crate::OtlpProtocol::Grpc));
        assert_eq!(telemetry.endpoint.as_deref(), Some("http://collector:4317"));
        assert_eq!(telemetry.headers["authorization"], "Bearer x");
        assert_eq!(
            telemetry.metrics_interval.as_duration(),
            Duration::from_secs(10)
        );
        assert_eq!(
            telemetry.logs_interval.as_duration(),
            Duration::from_secs(5)
        );
        assert_eq!(telemetry.temporality, crate::Temporality::Delta);
        assert_eq!(telemetry.prefix, "claude_code");
        assert!(!telemetry.log_user_prompts);
        assert!(parse("telemetry:\n  protocol: http\n").is_err());
    }

    #[test]
    fn reports_invalid_values() {
        let cases = [
            ("version: 2\n", "version"),
            ("relay:\n  enabled: true\n", "relay.url"),
            ("relay:\n  enabled: true\n  url: https://x\n", "relay.url"),
            (
                "instructions:\n  system_prompt: \"  \"\n",
                "instructions.system_prompt",
            ),
            ("llm:\n  default_model: gpt-5\n", "llm.default_model"),
            ("llm:\n  default_model: /model\n", "llm.default_model"),
            ("llm:\n  default_model: provider/\n", "llm.default_model"),
            ("llm:\n  providers:\n    a/b: {}\n", "llm.providers"),
            (
                "policy:\n  rules:\n  - tool: read\n    action: allow\n    command_prefix: [ls]\n",
                "policy.rules[0].command_prefix",
            ),
            (
                "context:\n  compaction_threshold: 1.5\n",
                "context.compaction_threshold",
            ),
            (
                "tools:\n  read:\n    max_lines: 0\n",
                "tools.read.max_lines",
            ),
            ("tools:\n  shell:\n    timeout: 0s\n", "tools.shell.timeout"),
            (
                "tools:\n  notes:\n    max_bytes: 0\n",
                "tools.notes.max_bytes",
            ),
            (
                "tools:\n  tasks:\n    max_items: 0\n",
                "tools.tasks.max_items",
            ),
            (
                "tools:\n  comment_guard:\n    allow_patterns: [\"(\"]\n",
                "tools.comment_guard.allow_patterns[0]",
            ),
            ("allowed_roots:\n- relative/path\n", "allowed_roots[0]"),
            (
                "tools:\n  allowed_dirs:\n  - /tmp\n  - relative\n",
                "tools.allowed_dirs[1]",
            ),
            (
                "telemetry:\n  endpoint: localhost:4318\n",
                "telemetry.endpoint",
            ),
            (
                "telemetry:\n  metrics_interval: 0s\n",
                "telemetry.metrics_interval",
            ),
            ("telemetry:\n  prefix: \"\"\n", "telemetry.prefix"),
            (
                "environment:\n  exclude: [\" \"]\n",
                "environment.exclude[0]",
            ),
            (
                "environment:\n  set:\n    \"A=B\": x\n",
                "environment.set.A=B",
            ),
            (
                "environment:\n  redaction:\n    allow: [\"\"]\n",
                "environment.redaction.allow[0]",
            ),
            ("environment:\n  env_file: \"\"\n", "environment.env_file"),
        ];
        for (text, field) in cases {
            match parse(text) {
                Err(ConfigError::Invalid { field: actual, .. }) => {
                    assert_eq!(actual, field, "for {text:?}")
                }
                other => panic!("expected invalid {field} for {text:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn default_yaml_round_trips() {
        let yaml = AgentConfig::default().to_yaml();
        let parsed = parse(&yaml).unwrap();
        let mut expected = AgentConfig::default();
        expected.expand_paths(&dirs().home);
        assert_eq!(parsed, expected);
    }

    #[test]
    fn presets_name_accounts_and_secrets_and_allow_by_default() {
        let config = AgentConfig::parse_preset(
            "llm:\n  accounts:\n    anthropic: work\nenvironment:\n  secrets:\n    NPM_TOKEN: npm-work\n",
            "preset work",
            &dirs(),
        )
        .unwrap();
        assert_eq!(config.llm.accounts["anthropic"], "work");
        assert_eq!(config.environment.secrets["NPM_TOKEN"], "npm-work");
        assert_eq!(config.policy.default, PolicyAction::Allow);

        let asks = AgentConfig::parse_preset("policy:\n  default: ask\n", "p", &dirs()).unwrap();
        assert_eq!(asks.policy.default, PolicyAction::Ask);
        assert_eq!(
            AgentConfig::parse_preset("", "p", &dirs())
                .unwrap()
                .policy
                .default,
            PolicyAction::Allow
        );
        assert_eq!(parse("").unwrap().policy.default, PolicyAction::Ask);
        for text in [
            "llm:\n  accounts:\n    anthropic: work\n",
            "environment:\n  secrets:\n    NPM_TOKEN: npm\n",
        ] {
            assert!(
                parse(text)
                    .unwrap_err()
                    .to_string()
                    .contains("only a preset")
            );
        }
    }

    #[test]
    fn presets_leave_the_managed_sections_to_the_agent() {
        for (text, field) in [
            ("storage:\n  data_dir: /x\n", "storage"),
            ("relay:\n  url: wss://x\n", "relay.url"),
            ("environment:\n  env_file: .env\n", "environment.env_file"),
            ("allowed_roots: [/]\n", "allowed_roots"),
        ] {
            match AgentConfig::parse_preset(text, "p", &dirs()) {
                Err(ConfigError::Invalid { field: actual, .. }) => assert_eq!(actual, field),
                other => panic!("expected invalid {field}, got {other:?}"),
            }
        }
        for (text, field) in [
            (
                "environment:\n  secrets:\n    'A B': x\n",
                "environment.secrets.A B",
            ),
            (
                "llm:\n  accounts:\n    anthropic: ''\n",
                "llm.accounts.anthropic",
            ),
        ] {
            match AgentConfig::parse_preset(text, "p", &dirs()) {
                Err(ConfigError::Invalid { field: actual, .. }) => assert_eq!(actual, field),
                other => panic!("expected invalid {field}, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_loaded_preset_gets_the_managed_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "llm:\n  default_reasoning_effort: high\n").unwrap();
        let settings = ManagedSettings {
            relay_url: "wss://oracle.test".into(),
            allowed_roots: vec![PathBuf::from("/workspaces")],
            workspace: PathBuf::from("/workspaces/app"),
            env_file: PathBuf::from("/home/u/.local/share/loom/secrets.env"),
        };
        let config = AgentConfig::load_preset(&path, &dirs(), &settings).unwrap();
        assert!(config.relay.enabled);
        assert_eq!(config.relay.url.as_deref(), Some("wss://oracle.test"));
        assert_eq!(config.allowed_roots, settings.allowed_roots);
        assert_eq!(
            config.lifecycle.default_workspace,
            Some(settings.workspace.clone())
        );
        assert_eq!(config.environment.env_file, Some(settings.env_file.clone()));
        assert_eq!(config.policy.default, PolicyAction::Allow);
        let missing =
            AgentConfig::load_preset(&dir.path().join("none.yaml"), &dirs(), &settings).unwrap();
        assert!(missing.relay.enabled);
    }
}
