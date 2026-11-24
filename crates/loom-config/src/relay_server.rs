use std::net::SocketAddr;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent::LoggingConfig;
use crate::error::ConfigError;
use crate::units::{ByteSize, HumanDuration};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RelayServerConfig {
    pub version: u32,
    pub listen: String,
    pub metrics_listen: Option<String>,
    pub tls: Option<TlsConfig>,
    pub allowed_hosts: Vec<String>,
    pub cloudflare_access: Option<AccessVerifyConfig>,
    /// Sending agents' notifications to clients' push services.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<PushConfig>,
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
            push: None,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PushConfig {
    /// Firebase Cloud Messaging, which delivers to the Android app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fcm: Option<FcmConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FcmConfig {
    /// The JSON key of a service account of the Firebase project the app
    /// was built with, from Project settings > Service accounts > Generate
    /// new private key.
    pub service_account: PathBuf,
    /// The FCM API's base URL; for tests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

pub const FCM_API_URL: &str = "https://fcm.googleapis.com";

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
    pub fn listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        self.listen
            .parse()
            .map_err(|_| ConfigError::invalid("oracle.yaml", "listen", "expected HOST:PORT"))
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

#[cfg(test)]
mod tests {
    use super::*;

    type Change = fn(&mut RelayServerConfig);

    #[test]
    fn rejects_invalid_values() {
        let cases: [(Change, &str); 4] = [
            (|c| c.listen = "nope".into(), "listen"),
            (|c| c.allowed_hosts = vec!["abc".into()], "allowed_hosts[0]"),
            (
                |c| {
                    c.cloudflare_access = Some(AccessVerifyConfig {
                        team_domain: "https://x".into(),
                        audience: "a".into(),
                        certs_url: None,
                    })
                },
                "cloudflare_access.team_domain",
            ),
            (|c| c.limits.queue_frames = 0, "limits.queue_frames"),
        ];
        for (change, field) in cases {
            let mut config = RelayServerConfig::default();
            change(&mut config);
            match config.validate("oracle.yaml") {
                Err(ConfigError::Invalid { field: actual, .. }) => assert_eq!(actual, field),
                other => panic!("expected {field}, got {other:?}"),
            }
        }
    }
}
