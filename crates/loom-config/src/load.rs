use std::path::Path;

use serde_json::Value;

use crate::agent::AgentConfig;
use crate::dirs::Dirs;
use crate::error::ConfigError;

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
        let parse_error = |err: serde_saphyr::Error| ConfigError::Parse {
            origin: origin.to_string(),
            message: err.to_string(),
        };
        let document: Value = serde_saphyr::from_str(text).map_err(parse_error)?;
        let mut config = if document.is_null() {
            Self::default()
        } else {
            serde_saphyr::from_str::<Self>(text).map_err(parse_error)?
        };
        config.expand_paths(&dirs.home);
        config.validate(origin)?;
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
    use crate::agent::{LogLevel, OpenAiAuth};
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
        assert_eq!(config.policy.rules.len(), 3);
        assert_eq!(config.policy.rules[1].command_prefix.len(), 4);
    }

    #[test]
    fn empty_file_uses_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.allowed_roots, [PathBuf::from("/home/u")]);
        assert_eq!(
            config.llm.providers.openai.unwrap().auth,
            OpenAiAuth::Chatgpt
        );
        assert!(config.llm.providers.openai_codex.is_some());
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
    fn rejects_removed_workspace_section() {
        let err = parse("workspaces:\n- path: ~/x\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn disables_provider_with_null() {
        let config = parse("llm:\n  providers:\n    openai-codex: null\n").unwrap();
        assert!(config.llm.providers.openai_codex.is_none());
        assert!(config.llm.providers.openai.is_some());
    }

    #[test]
    fn reports_invalid_values() {
        let cases = [
            ("version: 2\n", "version"),
            ("relay:\n  enabled: true\n", "relay.url"),
            ("relay:\n  enabled: true\n  url: https://x\n", "relay.url"),
            ("llm:\n  default_model: gpt-5\n", "llm.default_model"),
            ("llm:\n  default_model: other/gpt-5\n", "llm.default_model"),
            (
                "llm:\n  default_model: openai/gpt-5\n  providers:\n    openai: null\n",
                "llm.default_model",
            ),
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
            ("allowed_roots:\n- relative/path\n", "allowed_roots[0]"),
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
}
