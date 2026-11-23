use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::LoggingConfig;
use crate::dirs::{Dirs, expand_home};
use crate::error::ConfigError;
use crate::load::parse_yaml;
use crate::relay_server::{
    AccessVerifyConfig, PushConfig, RelayLimits, RelayServerConfig, TlsConfig,
};

pub const ORACLE_CONFIG_FILE: &str = "oracle.yaml";
pub const ORACLE_CONFIG_ENV: &str = "LOOM_ORACLE_CONFIG";
pub const ORACLE_DATA_DIR: &str = "oracle";
pub const ORACLE_MASTER_KEY_FILE: &str = "master.key";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OracleConfig {
    pub version: u32,
    pub url: Option<String>,
    pub listen: String,
    pub metrics_listen: Option<String>,
    pub tls: Option<TlsConfig>,
    pub cloudflare_access: Option<AccessVerifyConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<PushConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_key: Option<PathBuf>,
    pub limits: RelayLimits,
    pub logging: LoggingConfig,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            version: crate::CONFIG_VERSION,
            url: None,
            listen: "127.0.0.1:8090".into(),
            metrics_listen: None,
            tls: None,
            cloudflare_access: None,
            push: None,
            data_dir: None,
            master_key: None,
            limits: RelayLimits::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl OracleConfig {
    pub fn default_path(dirs: &Dirs) -> PathBuf {
        dirs.config_dir().join(ORACLE_CONFIG_FILE)
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

    fn expand_paths(&mut self, home: &Path) {
        if let Some(tls) = &mut self.tls {
            tls.cert = expand_home(&tls.cert, home);
            tls.key = expand_home(&tls.key, home);
        }
        if let Some(fcm) = self.push.as_mut().and_then(|p| p.fcm.as_mut()) {
            fcm.service_account = expand_home(&fcm.service_account, home);
        }
        for path in [&mut self.data_dir, &mut self.master_key]
            .into_iter()
            .flatten()
        {
            *path = expand_home(path, home);
        }
    }

    pub fn data_dir(&self, dirs: &Dirs) -> PathBuf {
        self.data_dir
            .clone()
            .unwrap_or_else(|| dirs.default_data_dir().join(ORACLE_DATA_DIR))
    }

    pub fn master_key_path(&self, dirs: &Dirs) -> PathBuf {
        self.master_key
            .clone()
            .unwrap_or_else(|| self.data_dir(dirs).join(ORACLE_MASTER_KEY_FILE))
    }

    pub fn public_url(&self) -> String {
        match &self.url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None if self.tls.is_some() => format!("wss://{}", self.listen),
            None => format!("ws://{}", self.listen),
        }
    }

    pub fn relay_config(&self) -> RelayServerConfig {
        RelayServerConfig {
            version: self.version,
            listen: self.listen.clone(),
            metrics_listen: self.metrics_listen.clone(),
            tls: self.tls.clone(),
            allowed_hosts: Vec::new(),
            cloudflare_access: self.cloudflare_access.clone(),
            push: self.push.clone(),
            limits: self.limits.clone(),
            logging: self.logging.clone(),
        }
    }

    pub fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: &str, message: &str| ConfigError::invalid(origin, field, message);
        if let Some(url) = &self.url {
            let authority = url
                .strip_prefix("wss://")
                .or_else(|| url.strip_prefix("ws://"));
            match authority {
                Some(rest) if !rest.trim_end_matches('/').is_empty() => {
                    if rest.contains(['?', '#', ' ']) {
                        return Err(invalid("url", "must not have a query or a fragment"));
                    }
                }
                _ => {
                    return Err(invalid(
                        "url",
                        "expected a URL such as wss://oracle.example.com",
                    ));
                }
            }
        }
        for (field, path) in [
            ("data_dir", &self.data_dir),
            ("master_key", &self.master_key),
        ] {
            if let Some(path) = path
                && !path.is_absolute()
            {
                return Err(invalid(field, "expected an absolute path or a path in ~"));
            }
        }
        self.relay_config().validate(origin)
    }
}

pub fn oracle_config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(OracleConfig)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::ByteSize;

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
        let config = OracleConfig::parse(
            include_str!("../tests/fixtures/oracle.yaml"),
            "oracle.yaml",
            &dirs(),
        )
        .unwrap();
        assert_eq!(config.public_url(), "wss://oracle.example.com");
        assert_eq!(
            config.data_dir(&dirs()),
            PathBuf::from("/var/lib/loom-oracle")
        );
        assert_eq!(
            config.master_key_path(&dirs()),
            PathBuf::from("/run/credentials/loom-oracle.service/master.key")
        );
        assert_eq!(config.limits.max_frame, ByteSize::kib(256));
        let relay = config.relay_config();
        assert!(relay.allowed_hosts.is_empty());
        assert_eq!(relay.listen, "127.0.0.1:8090");
        assert_eq!(
            relay.push.and_then(|p| p.fcm).unwrap().service_account,
            PathBuf::from("/home/u/firebase-service-account.json")
        );

        let empty = OracleConfig::parse("", "oracle.yaml", &dirs()).unwrap();
        assert_eq!(empty, OracleConfig::default());
        assert_eq!(empty.public_url(), "ws://127.0.0.1:8090");
        assert_eq!(
            empty.master_key_path(&dirs()),
            PathBuf::from("/home/u/.local/share/loom/oracle/master.key")
        );
        let yaml = OracleConfig::default().to_yaml();
        assert_eq!(
            OracleConfig::parse(&yaml, "oracle.yaml", &dirs()).unwrap(),
            OracleConfig::default()
        );
        let home = OracleConfig::parse("data_dir: ~/oracle\n", "oracle.yaml", &dirs()).unwrap();
        assert_eq!(home.data_dir(&dirs()), PathBuf::from("/home/u/oracle"));
    }

    #[test]
    fn rejects_invalid_values() {
        for (text, field) in [
            ("url: https://oracle.example.com\n", "url"),
            ("url: wss://\n", "url"),
            ("url: wss://oracle.example.com/?a=b\n", "url"),
            ("data_dir: relative\n", "data_dir"),
            ("master_key: key\n", "master_key"),
            ("listen: nope\n", "listen"),
            ("limits:\n  burst: 1\n", "limits.burst"),
        ] {
            match OracleConfig::parse(text, "oracle.yaml", &dirs()) {
                Err(ConfigError::Invalid { field: actual, .. }) => assert_eq!(actual, field),
                other => panic!("expected {field} for {text:?}, got {other:?}"),
            }
        }
        assert!(OracleConfig::parse("allowed_hosts: []\n", "oracle.yaml", &dirs()).is_err());
    }

    #[test]
    fn schema_describes_the_sections() {
        let schema = oracle_config_schema();
        let properties = schema["properties"].as_object().unwrap();
        for section in ["url", "listen", "data_dir", "master_key", "limits"] {
            assert!(properties.contains_key(section), "missing {section}");
        }
        assert!(!properties.contains_key("allowed_hosts"));
    }
}
