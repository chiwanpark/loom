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
pub const DEFAULT_INCUS_PROJECT: &str = "loom";
pub const DEFAULT_INCUS_POOL: &str = "default";
pub const DEFAULT_INCUS_NETWORK: &str = "incusbr0";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostDriver {
    #[default]
    Incus,
    Process,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessDriverConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IncusDriverConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_certificate: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_certificate: Option<PathBuf>,
    pub project: String,
    pub pool: String,
    pub network: String,
}

impl Default for IncusDriverConfig {
    fn default() -> Self {
        Self {
            socket: None,
            url: None,
            client_certificate: None,
            client_key: None,
            server_certificate: None,
            project: DEFAULT_INCUS_PROJECT.into(),
            pool: DEFAULT_INCUS_POOL.into(),
            network: DEFAULT_INCUS_NETWORK.into(),
        }
    }
}

pub struct IncusCertificates {
    pub client_certificate: PathBuf,
    pub client_key: PathBuf,
    pub server_certificate: PathBuf,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub directories: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_user: Option<HostUser>,
    #[serde(skip_serializing_if = "NetworkRules::is_default")]
    pub network: NetworkRules,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AclAction {
    #[default]
    Allow,
    Reject,
    Drop,
}

impl AclAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Reject => "reject",
            Self::Drop => "drop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkRules {
    pub isolate: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub acls: Vec<String>,
    pub default_egress: AclAction,
    pub default_ingress: AclAction,
}

impl Default for NetworkRules {
    fn default() -> Self {
        Self {
            isolate: true,
            acls: Vec::new(),
            default_egress: AclAction::Allow,
            default_ingress: AclAction::Allow,
        }
    }
}

impl NetworkRules {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostUser {
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    pub driver: HostDriver,
    pub incus: IncusDriverConfig,
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
            incus: IncusDriverConfig::default(),
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

    fn paths(&mut self) -> [(&'static str, &mut Option<PathBuf>); 7] {
        [
            ("data_dir", &mut self.data_dir),
            ("process.root", &mut self.process.root),
            ("agent.path", &mut self.agent.path),
            ("incus.socket", &mut self.incus.socket),
            (
                "incus.client_certificate",
                &mut self.incus.client_certificate,
            ),
            ("incus.client_key", &mut self.incus.client_key),
            (
                "incus.server_certificate",
                &mut self.incus.server_certificate,
            ),
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

    pub fn agent_cache(&self, dirs: &Dirs) -> PathBuf {
        self.data_dir(dirs).join("agents")
    }

    pub fn incus_dir(&self, dirs: &Dirs) -> PathBuf {
        self.data_dir(dirs).join("incus")
    }

    pub fn incus_certificates(&self, dirs: &Dirs) -> IncusCertificates {
        let dir = self.incus_dir(dirs);
        let pick =
            |path: &Option<PathBuf>, file: &str| path.clone().unwrap_or_else(|| dir.join(file));
        IncusCertificates {
            client_certificate: pick(&self.incus.client_certificate, "client.crt"),
            client_key: pick(&self.incus.client_key, "client.key"),
            server_certificate: pick(&self.incus.server_certificate, "server.crt"),
        }
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
        if let Some(url) = &self.incus.url
            && !url.starts_with("https://")
        {
            return Err(invalid("incus.url", "expected an https:// URL"));
        }
        if self.incus.url.is_some() && self.incus.socket.is_some() {
            return Err(invalid(
                "incus.socket",
                "set incus.url or incus.socket, not both",
            ));
        }
        for (field, value) in [
            ("incus.project", &self.incus.project),
            ("incus.pool", &self.incus.pool),
            ("incus.network", &self.incus.network),
        ] {
            if value.is_empty() || value.len() > 63 || value.contains(['/', '?', '&', ' ']) {
                return Err(invalid(field, "expected a name of Incus"));
            }
        }
        if self.rules.max_codespaces == Some(0) {
            return Err(invalid("rules.max_codespaces", "must be 1 or more"));
        }
        if let Some(user) = self.rules.host_user
            && (user.uid == 0 || user.gid == 0 || user.uid == u32::MAX || user.gid == u32::MAX)
        {
            return Err(invalid(
                "rules.host_user",
                "expected a non-root UID and GID",
            ));
        }
        for directory in &self.rules.directories {
            let text = directory.to_string_lossy();
            let valid = directory.is_absolute()
                && text != "/"
                && text.len() <= 200
                && !text.ends_with('/')
                && !text.contains("//")
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
                && !text.split('/').any(|part| part == "." || part == "..");
            if !valid {
                return Err(invalid(
                    "rules.directories",
                    "expected absolute paths below /",
                ));
            }
        }
        let network = &self.rules.network;
        for acl in &network.acls {
            let valid = !acl.is_empty()
                && acl.len() <= 63
                && acl
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && !acl.starts_with('-');
            if !valid || network.acls.iter().filter(|a| *a == acl).count() > 1 {
                return Err(invalid(
                    "rules.network.acls",
                    "expected different names of Incus network ACLs",
                ));
            }
        }
        if network.acls.is_empty()
            && (network.default_egress != AclAction::Allow
                || network.default_ingress != AclAction::Allow)
        {
            return Err(invalid(
                "rules.network.acls",
                "the default actions apply only with network ACLs; name at least one",
            ));
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
        assert_eq!(config.driver, HostDriver::Incus);
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
        assert_eq!(
            config.rules.directories,
            vec![PathBuf::from("/srv/projects")]
        );
        assert_eq!(
            config.rules.host_user,
            Some(HostUser {
                uid: 1000,
                gid: 1000
            })
        );
        assert!(config.rules.network.isolate);
        assert_eq!(config.rules.network.acls, vec!["loom-codespaces"]);
        assert_eq!(
            config.incus.url.as_deref(),
            Some("https://incus.example.com:8443")
        );
        assert_eq!(config.incus.pool, "ssd");
        let certificates = config.incus_certificates(&dirs());
        assert_eq!(
            certificates.client_key,
            PathBuf::from("/var/lib/loom-host/incus/client.key")
        );
        assert_eq!(
            certificates.server_certificate,
            PathBuf::from("/var/lib/loom-host/incus/server.crt")
        );

        let empty = HostConfig::parse("", "host.yaml", &dirs()).unwrap();
        assert_eq!(empty, HostConfig::default());
        assert_eq!(empty.incus.project, DEFAULT_INCUS_PROJECT);
        assert!(empty.rules.network.isolate);
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
            ("rules:\n  directories: [srv]\n", "rules.directories"),
            (
                "rules:\n  directories: [/srv/../etc]\n",
                "rules.directories",
            ),
            ("report_interval: 1s\n", "report_interval"),
            ("version: 9\n", "version"),
            ("incus:\n  url: http://x:8443\n", "incus.url"),
            (
                "incus:\n  url: https://x\n  socket: /run/incus/unix.socket\n",
                "incus.socket",
            ),
            ("incus:\n  project: a/b\n", "incus.project"),
            ("incus:\n  pool: ''\n", "incus.pool"),
            ("incus:\n  client_key: key.pem\n", "incus.client_key"),
            (
                "rules:\n  network:\n    acls: [a, a]\n",
                "rules.network.acls",
            ),
            (
                "rules:\n  network:\n    acls: ['a b']\n",
                "rules.network.acls",
            ),
            (
                "rules:\n  network:\n    default_egress: reject\n",
                "rules.network.acls",
            ),
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
        for section in ["driver", "data_dir", "incus", "agent", "rules", "logging"] {
            assert!(properties.contains_key(section), "missing {section}");
        }
    }
}
