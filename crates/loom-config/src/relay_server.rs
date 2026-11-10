use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::LoggingConfig;
use crate::dirs::{Dirs, expand_home};
use crate::error::ConfigError;
use crate::load::parse_yaml;
use crate::units::{ByteSize, HumanDuration};

pub const RELAY_CONFIG_FILE: &str = "relay.yaml";
pub const RELAY_CONFIG_ENV: &str = "LOOM_RELAY_CONFIG";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RelayServerConfig {
    pub version: u32,
    pub listen: String,
    pub metrics_listen: Option<String>,
    pub tls: Option<TlsConfig>,
    pub allowed_hosts: Vec<String>,
    pub cloudflare_access: Option<AccessVerifyConfig>,
    pub limits: RelayLimits,
    pub logging: LoggingConfig,
}

impl Default for RelayServerConfig {
    fn default() -> Self {
        Self {
            version: crate::CONFIG_VERSION,
            listen: "127.0.0.1:8080".into(),
            metrics_listen: None,
            tls: None,
            allowed_hosts: Vec::new(),
            cloudflare_access: None,
            limits: RelayLimits::default(),
            logging: LoggingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessVerifyConfig {
    pub team_domain: String,
    pub audience: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certs_url: Option<String>,
}

impl AccessVerifyConfig {
    pub fn issuer(&self) -> String {
        format!("https://{}", self.team_domain.trim_end_matches('/'))
    }

    pub fn certs_url(&self) -> String {
        self.certs_url
            .clone()
            .unwrap_or_else(|| format!("{}/cdn-cgi/access/certs", self.issuer()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RelayLimits {
    pub max_frame: ByteSize,
    pub max_channels_per_agent: usize,
    pub max_agents_per_host: usize,
    pub max_connections_per_ip: usize,
    pub messages_per_second: u32,
    pub burst: u32,
    pub idle_timeout: HumanDuration,
    pub handshake_timeout: HumanDuration,
    pub queue_frames: usize,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self {
            max_frame: ByteSize::kib(256),
            max_channels_per_agent: 64,
            max_agents_per_host: 32,
            max_connections_per_ip: 64,
            messages_per_second: 500,
            burst: 2000,
            idle_timeout: HumanDuration::from_secs(90),
            handshake_timeout: HumanDuration::from_secs(10),
            queue_frames: 256,
        }
    }
}

impl RelayServerConfig {
    pub fn default_path(dirs: &Dirs) -> PathBuf {
        dirs.config_dir().join(RELAY_CONFIG_FILE)
    }

    pub fn load(path: &Path, dirs: &Dirs) -> Result<Self, ConfigError> {
        let origin = path.display().to_string();
        let mut config: Self = match std::fs::read_to_string(path) {
            Ok(text) => parse_yaml(&text, &origin)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        config.expand_paths(&dirs.home);
        config.validate(&origin)?;
        Ok(config)
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
    }

    pub fn listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        self.listen
            .parse()
            .map_err(|_| ConfigError::invalid("relay.yaml", "listen", "expected HOST:PORT"))
    }

    pub fn validate(&self, origin: &str) -> Result<(), ConfigError> {
        let invalid = |field: &str, message: String| ConfigError::invalid(origin, field, message);
        if self.version != crate::CONFIG_VERSION {
            return Err(invalid(
                "version",
                format!("unsupported version {}", self.version),
            ));
        }
        if self.listen.parse::<SocketAddr>().is_err() {
            return Err(invalid(
                "listen",
                format!("{:?} is not HOST:PORT", self.listen),
            ));
        }
        if let Some(metrics) = &self.metrics_listen
            && metrics.parse::<SocketAddr>().is_err()
        {
            return Err(invalid(
                "metrics_listen",
                format!("{metrics:?} is not HOST:PORT"),
            ));
        }
        for (index, host) in self.allowed_hosts.iter().enumerate() {
            let ok = host.len() == 64 && host.chars().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err(invalid(
                    &format!("allowed_hosts[{index}]"),
                    "expected a 64 character host ID".into(),
                ));
            }
        }
        if let Some(access) = &self.cloudflare_access {
            if access.team_domain.trim().is_empty() || access.team_domain.contains("://") {
                return Err(invalid(
                    "cloudflare_access.team_domain",
                    "expected a domain such as myteam.cloudflareaccess.com".into(),
                ));
            }
            if access.audience.trim().is_empty() {
                return Err(invalid(
                    "cloudflare_access.audience",
                    "must not be empty".into(),
                ));
            }
        }
        let limits = &self.limits;
        let positive = [
            ("limits.max_frame", limits.max_frame.0 >= 1024),
            (
                "limits.max_channels_per_agent",
                limits.max_channels_per_agent > 0,
            ),
            ("limits.max_agents_per_host", limits.max_agents_per_host > 0),
            (
                "limits.max_connections_per_ip",
                limits.max_connections_per_ip > 0,
            ),
            ("limits.messages_per_second", limits.messages_per_second > 0),
            ("limits.burst", limits.burst >= limits.messages_per_second),
            (
                "limits.idle_timeout",
                !limits.idle_timeout.as_duration().is_zero(),
            ),
            (
                "limits.handshake_timeout",
                !limits.handshake_timeout.as_duration().is_zero(),
            ),
            ("limits.queue_frames", limits.queue_frames > 0),
        ];
        for (field, ok) in positive {
            if !ok {
                return Err(invalid(field, "is out of range".into()));
            }
        }
        Ok(())
    }
}

pub fn relay_config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(RelayServerConfig)).unwrap_or_default()
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
        let config = RelayServerConfig::parse(
            include_str!("../tests/fixtures/relay.yaml"),
            "relay.yaml",
            &dirs(),
        )
        .unwrap();
        assert_eq!(config.allowed_hosts.len(), 1);
        assert_eq!(
            config.cloudflare_access.unwrap().certs_url(),
            "https://myteam.cloudflareaccess.com/cdn-cgi/access/certs"
        );
        assert_eq!(config.limits.max_frame, ByteSize::kib(256));
        let empty = RelayServerConfig::parse("", "relay.yaml", &dirs()).unwrap();
        assert_eq!(empty, RelayServerConfig::default());
        let yaml = RelayServerConfig::default().to_yaml();
        assert_eq!(
            RelayServerConfig::parse(&yaml, "relay.yaml", &dirs()).unwrap(),
            RelayServerConfig::default()
        );
    }

    #[test]
    fn rejects_invalid_values() {
        for (text, field) in [
            ("listen: nope\n", "listen"),
            ("allowed_hosts:\n- abc\n", "allowed_hosts[0]"),
            (
                "cloudflare_access:\n  team_domain: https://x\n  audience: a\n",
                "cloudflare_access.team_domain",
            ),
            ("limits:\n  queue_frames: 0\n", "limits.queue_frames"),
        ] {
            match RelayServerConfig::parse(text, "relay.yaml", &dirs()) {
                Err(ConfigError::Invalid { field: actual, .. }) => assert_eq!(actual, field),
                other => panic!("expected {field} for {text:?}, got {other:?}"),
            }
        }
    }
}
