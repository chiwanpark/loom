use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRoute {
    #[default]
    Direct,
    Hosts,
    #[serde(other)]
    Unknown,
}

impl ProviderRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Hosts => "hosts",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "direct" => Self::Direct,
            "hosts" => Self::Hosts,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for ProviderRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitProvider {
    pub name: String,
    pub web_url: String,
    pub api_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_email: Option<String>,
    #[serde(default)]
    pub route: ProviderRoute,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitProviderInput {
    pub name: String,
    pub web_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_email: Option<String>,
    #[serde(default)]
    pub route: ProviderRoute,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCredential {
    pub name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl GitCredential {
    pub fn scope(&self) -> String {
        match (&self.owner, &self.repository) {
            (Some(owner), Some(repository)) => format!("{owner}/{repository}"),
            (Some(owner), None) => format!("{owner}/*"),
            _ => "*".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCredentialInput {
    pub name: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Secret>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub yaml: String,
    #[serde(default)]
    pub git_credentials: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRepository {
    pub owner: String,
    pub name: String,
    pub url: String,
    pub web_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySearch {
    pub repositories: Vec<RemoteRepository>,
    #[serde(default)]
    pub more: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branches {
    pub branches: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitToken {
    pub username: String,
    pub password: Secret,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitTokenResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<GitToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhLogin {
    pub user: String,
    pub token: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitProviderSetup {
    pub name: String,
    pub web_url: String,
    pub api_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_email: Option<String>,
    #[serde(default)]
    pub credentials: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gh: Option<GhLogin>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSetup {
    #[serde(default)]
    pub providers: Vec<GitProviderSetup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEndpoint {
    pub name: String,
    pub api_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCheck {
    #[serde(default)]
    pub providers: Vec<ProviderEndpoint>,
    #[serde(default)]
    pub directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCheckResult {
    #[serde(default)]
    pub providers: Vec<CheckResult>,
    #[serde(default)]
    pub directories: Vec<CheckResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub api_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Secret>,
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitProviders {
    pub providers: Vec<GitProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitProviderResult {
    pub provider: GitProvider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCredentials {
    pub credentials: Vec<GitCredential>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCredentialResult {
    pub credential: GitCredential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presets {
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetResult {
    pub preset: Preset,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn secrets_do_not_show_in_debug_output() {
        let input = GitCredentialInput {
            name: "work".into(),
            provider: "github".into(),
            owner: None,
            repository: None,
            token: Some(Secret::new("ghp_secret")),
        };
        assert!(!format!("{input:?}").contains("ghp_secret"));
        assert_eq!(
            serde_json::to_value(&input).unwrap()["token"],
            json!("ghp_secret")
        );
    }

    #[test]
    fn routes_tolerate_new_values() {
        assert_eq!(ProviderRoute::parse("hosts"), ProviderRoute::Hosts);
        assert_eq!(
            serde_json::from_str::<ProviderRoute>("\"proxy\"").unwrap(),
            ProviderRoute::Unknown
        );
        let provider: GitProviderInput =
            serde_json::from_value(json!({"name": "github", "web_url": "https://x.test"})).unwrap();
        assert_eq!(provider.route, ProviderRoute::Direct);
    }

    #[test]
    fn credentials_show_their_scope() {
        let mut credential = GitCredential {
            name: "a".into(),
            provider: "github".into(),
            owner: None,
            repository: None,
            login: None,
            expires_at: None,
            checked_at: None,
            check_error: None,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(credential.scope(), "*");
        credential.owner = Some("acme".into());
        assert_eq!(credential.scope(), "acme/*");
        credential.repository = Some("server".into());
        assert_eq!(credential.scope(), "acme/server");
    }
}
