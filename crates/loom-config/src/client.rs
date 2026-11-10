use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::CloudflareAccessConfig;
use crate::dirs::Dirs;
use crate::error::ConfigError;
use crate::load::parse_yaml;

pub const CLIENT_CONFIG_FILE: &str = "client.yaml";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    pub version: u32,
    pub hosts: Vec<HostEntry>,
    pub appearance: AppearanceConfig,
    pub keymap: BTreeMap<String, String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            version: crate::CONFIG_VERSION,
            hosts: vec![HostEntry::Local {
                name: "local".into(),
            }],
            appearance: AppearanceConfig::default(),
            keymap: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostEntry {
    Local {
        name: String,
    },
    Ssh {
        name: String,
        destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_bin: Option<String>,
    },
    Relay {
        name: String,
        url: String,
        host_id: String,
        host_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cloudflare_access: Option<CloudflareAccessConfig>,
    },
}

impl HostEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Local { name } | Self::Ssh { name, .. } | Self::Relay { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceConfig {
    pub theme: Theme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_font: Option<String>,
    pub ui_font_size: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mono_font: Option<String>,
    pub mono_font_size: f32,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            theme_name: None,
            ui_font: None,
            ui_font_size: 15.0,
            mono_font: None,
            mono_font_size: 14.0,
        }
    }
}

impl ClientConfig {
    pub fn default_path(dirs: &Dirs) -> PathBuf {
        dirs.config_dir().join(CLIENT_CONFIG_FILE)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let origin = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &origin),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn parse(text: &str, origin: &str) -> Result<Self, ConfigError> {
        let config: Self = parse_yaml(text, origin)?;
        config.validate(origin)?;
        Ok(config)
    }

    pub fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: String, message: &str| ConfigError::invalid(origin, field, message);
        if self.version != crate::CONFIG_VERSION {
            return Err(invalid("version".into(), "unsupported version"));
        }
        let mut names = std::collections::BTreeSet::new();
        for (index, host) in self.hosts.iter().enumerate() {
            let field = |name: &str| format!("hosts[{index}].{name}");
            if host.name().trim().is_empty() {
                return Err(invalid(field("name"), "must not be empty"));
            }
            if !names.insert(host.name()) {
                return Err(invalid(field("name"), "is used by another host"));
            }
            match host {
                HostEntry::Local { .. } => {}
                HostEntry::Ssh { destination, .. } => {
                    if destination.trim().is_empty() || destination.starts_with('-') {
                        return Err(invalid(
                            field("destination"),
                            "is not a valid SSH destination",
                        ));
                    }
                }
                HostEntry::Relay {
                    url,
                    host_id,
                    host_key,
                    ..
                } => {
                    if !(url.starts_with("wss://") || url.starts_with("ws://")) {
                        return Err(invalid(field("url"), "must start with wss:// or ws://"));
                    }
                    if host_id.len() != 64 || !host_id.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(invalid(field("host_id"), "expected a 64 character host ID"));
                    }
                    if host_key.len() != 43 {
                        return Err(invalid(field("host_key"), "expected a base64url key"));
                    }
                }
            }
        }
        let sizes = [
            ("appearance.ui_font_size", self.appearance.ui_font_size),
            ("appearance.mono_font_size", self.appearance.mono_font_size),
        ];
        for (field, size) in sizes {
            if !(6.0..=72.0).contains(&size) {
                return Err(invalid(field.into(), "must be between 6 and 72"));
            }
        }
        Ok(())
    }

    pub fn to_yaml(&self) -> String {
        serde_saphyr::to_string(self).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, self.to_yaml())?;
        std::fs::rename(tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips_hosts() {
        let text = format!(
            "hosts:\n- kind: local\n  name: here\n- kind: ssh\n  name: box\n  destination: me@box\n- kind: relay\n  name: home\n  url: wss://relay.example.com\n  host_id: {}\n  host_key: {}\n  cloudflare_access:\n    client_id_env: ID\n    client_secret_env: SECRET\nappearance:\n  theme: light\n  theme_name: Gruvbox Light\n",
            "a".repeat(64),
            "b".repeat(43)
        );
        let config = ClientConfig::parse(&text, "client.yaml").unwrap();
        assert_eq!(config.hosts.len(), 3);
        assert_eq!(config.hosts[1].name(), "box");
        assert_eq!(config.appearance.theme, Theme::Light);
        assert_eq!(
            config.appearance.theme_name.as_deref(),
            Some("Gruvbox Light")
        );
        let again = ClientConfig::parse(&config.to_yaml(), "client.yaml").unwrap();
        assert_eq!(again, config);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loom/client.yaml");
        config.save(&path).unwrap();
        assert_eq!(ClientConfig::load(&path).unwrap(), config);
        assert_eq!(
            ClientConfig::load(&dir.path().join("missing.yaml")).unwrap(),
            ClientConfig::default()
        );
    }

    #[test]
    fn rejects_bad_hosts() {
        for text in [
            "hosts:\n- kind: local\n  name: a\n- kind: local\n  name: a\n",
            "hosts:\n- kind: ssh\n  name: a\n  destination: -oProxyCommand=x\n",
            "hosts:\n- kind: relay\n  name: a\n  url: https://x\n  host_id: x\n  host_key: y\n",
            "hosts:\n- kind: telnet\n  name: a\n",
            "appearance:\n  ui_font_size: 2\n",
        ] {
            assert!(ClientConfig::parse(text, "client.yaml").is_err(), "{text}");
        }
    }
}
