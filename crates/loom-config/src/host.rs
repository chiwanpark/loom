use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::LoggingConfig;
use crate::dirs::{Dirs, expand_home};
use crate::error::ConfigError;
use crate::load::parse_yaml;
use crate::units::HumanDuration;

pub const HOST_CONFIG_FILE: &str = "host.yaml";
pub const HOST_CONFIG_ENV: &str = "LOOM_HOST_CONFIG";
pub const HOST_DATA_DIR: &str = "host";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostDriver {
    #[default]
    Process,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessDriverConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HostAgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HostRules {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_codespaces: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    pub driver: HostDriver,
    pub process: ProcessDriverConfig,
    pub agent: HostAgentConfig,
    pub rules: HostRules,
    pub report_interval: HumanDuration,
    pub logging: LoggingConfig,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            version: crate::CONFIG_VERSION,
            data_dir: None,
            driver: HostDriver::default(),
            process: ProcessDriverConfig::default(),
            agent: HostAgentConfig::default(),
            rules: HostRules::default(),
            report_interval: HumanDuration::from_secs(60),
            logging: LoggingConfig::default(),
        }
    }
}

impl HostConfig {
    pub fn default_path(dirs: &Dirs) -> PathBuf {
        dirs.config_dir().join(HOST_CONFIG_FILE)
    }

    pub fn load(path: &Path, dirs: &Dirs) -> Result<Self, ConfigError> {
        let origin = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &origin, dirs),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Self::parse("", &origin, dirs)
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
        Ok(config)
    }

    pub fn to_yaml(&self) -> String {
        serde_saphyr::to_string(self).unwrap_or_default()
    }

    fn paths(&mut self) -> [(&'static str, &mut Option<PathBuf>); 3] {
        [
            ("data_dir", &mut self.data_dir),
            ("process.root", &mut self.process.root),
            ("agent.path", &mut self.agent.path),
        ]
    }

    fn expand_paths(&mut self, home: &Path) {
        for (_, path) in self.paths() {
            if let Some(path) = path {
                *path = expand_home(path, home);
            }
        }
    }

    pub fn data_dir(&self, dirs: &Dirs) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| dirs.default_data_dir().join(HOST_DATA_DIR))
    }

    pub fn process_root(&self, dirs: &Dirs) -> PathBuf {
        self.process
            .root
            .clone()
            .unwrap_or_else(|| self.data_dir(dirs).join("codespaces"))
    }

    pub fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: &str, message: &str| ConfigError::invalid(origin, field, message);
        if self.version != crate::CONFIG_VERSION {
            return Err(invalid(
                "version",
                &format!("expected version {}", crate::CONFIG_VERSION),
            ));
        }
        for (field, path) in self.clone().paths() {
            if let Some(path) = path
                && !path.is_absolute()
            {
                return Err(invalid(field, "expected an absolute path or a path in ~"));
            }
        }
        if self.rules.max_codespaces == Some(0) {
            return Err(invalid("rules.max_codespaces", "must be 1 or more"));
        }
        if self.report_interval.as_duration().as_secs() < 5 {
            return Err(invalid("report_interval", "must be 5s or more"));
        }
        Ok(())
    }
}

pub fn host_config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(HostConfig)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs {
            home: PathBuf::from("/home/u"),
            config_home: PathBuf::from("/home/u/.config"),
            data_home: PathBuf::from("/home/u/.local/share"),
            state_home: PathBuf::from("/home/u/.local/state"),
            runtime_dir: None,
        }
    }

    #[test]
    fn parses_the_example() {
        let config = HostConfig::parse(
            include_str!("../tests/fixtures/host.yaml"),
            "host.yaml",
            &dirs(),
        )
        .unwrap();
        assert_eq!(config.driver, HostDriver::Process);
        assert_eq!(
            config.data_dir(&dirs()),
            PathBuf::from("/var/lib/loom-host")
        );
        assert_eq!(
            config.process_root(&dirs()),
            PathBuf::from("/var/lib/loom-host/codespaces")
        );
        assert_eq!(
            config.agent.path,
            Some(PathBuf::from("/home/u/bin/loom-agent"))
        );
        assert_eq!(config.rules.max_codespaces, Some(8));

        let empty = HostConfig::parse("", "host.yaml", &dirs()).unwrap();
        assert_eq!(empty, HostConfig::default());
        assert_eq!(
            empty.data_dir(&dirs()),
            PathBuf::from("/home/u/.local/share/loom/host")
        );
        let yaml = HostConfig::default().to_yaml();
        assert_eq!(
            HostConfig::parse(&yaml, "host.yaml", &dirs()).unwrap(),
            HostConfig::default()
        );
    }

    #[test]
    fn rejects_invalid_values() {
        for (text, field) in [
            ("data_dir: relative\n", "data_dir"),
            ("agent:\n  path: loom-agent\n", "agent.path"),
            ("rules:\n  max_codespaces: 0\n", "rules.max_codespaces"),
            ("report_interval: 1s\n", "report_interval"),
            ("version: 9\n", "version"),
        ] {
            match HostConfig::parse(text, "host.yaml", &dirs()) {
                Err(ConfigError::Invalid { field: actual, .. }) => assert_eq!(actual, field),
                other => panic!("expected {field} for {text:?}, got {other:?}"),
            }
        }
        assert!(HostConfig::parse("driver: docker\n", "host.yaml", &dirs()).is_err());
        assert!(HostConfig::parse("rules:\n  images: []\n", "host.yaml", &dirs()).is_err());
    }

    #[test]
    fn schema_describes_the_sections() {
        let schema = host_config_schema();
        let properties = schema["properties"].as_object().unwrap();
        for section in ["driver", "data_dir", "agent", "rules", "logging"] {
            assert!(properties.contains_key(section), "missing {section}");
        }
    }
}
