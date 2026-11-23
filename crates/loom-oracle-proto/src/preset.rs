use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Secret;

pub const MAX_PRESET_YAML: usize = 256 * 1024;
pub const MAX_AGENTS_MD: usize = 256 * 1024;
pub const MAX_PRESET_FILES: usize = 200;
pub const MAX_PRESET_FILES_BYTES: usize = 512 * 1024;
pub const MAX_SECRET: usize = 64 * 1024;
pub const MAX_NOTES: usize = 256 * 1024;
pub const MAX_FIRST_MESSAGE: usize = 64 * 1024;
pub const SKILLS_DIR: &str = "skills";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    #[default]
    SignIn,
    ApiKey,
    #[serde(other)]
    Unknown,
}

impl AccountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::ApiKey => "api_key",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Self {
        match text {
            "sign_in" => Self::SignIn,
            "api_key" => Self::ApiKey,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for AccountKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAccount {
    pub name: String,
    pub provider: String,
    pub kind: AccountKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelLoginMethod {
    Browser {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redirect_uri: Option<String>,
    },
    DeviceCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelLoginPrompt {
    OpenUrl {
        url: String,
        redirect_uri: String,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        expires_in_secs: u64,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelLoginInput {
    Redirect { url: String },
    Code { code: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLogin {
    pub login: String,
    pub account: String,
    pub provider: String,
    pub prompt: ModelLoginPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAccounts {
    pub accounts: Vec<ModelAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAccountResult {
    pub account: ModelAccount,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Models {
    pub models: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCredentials {
    #[serde(default)]
    pub sections: BTreeMap<String, Value>,
    #[serde(default)]
    pub accounts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl fmt::Debug for ModelCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelCredentials")
            .field("providers", &self.sections.keys().collect::<Vec<_>>())
            .field("accounts", &self.accounts)
            .field("valid_secs", &self.valid_secs)
            .field("errors", &self.errors)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secrets {
    pub secrets: Vec<SecretInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretResult {
    pub secret: SecretInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetDetail {
    pub preset: crate::Preset,
    #[serde(default)]
    pub files: Vec<PresetFile>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetBundle {
    pub name: String,
    pub yaml: String,
    #[serde(default)]
    pub agents_md: String,
    #[serde(default)]
    pub files: Vec<PresetFile>,
    #[serde(default)]
    pub secrets: BTreeMap<String, Secret>,
    #[serde(default)]
    pub accounts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceNotes {
    pub workspace: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub state: String,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodespaceSession {
    pub codespace: String,
    pub workspace: String,
    pub session: SessionSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sessions {
    pub sessions: Vec<CodespaceSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirstMessageResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<FirstMessage>,
}

pub fn valid_preset_path(path: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some(SKILLS_DIR)
        && path.split('/').count() >= 3
        && path.len() <= 300
        && parts.all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 100
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn preset_files_are_in_skill_directories() {
        for path in ["skills/review/SKILL.md", "skills/a/scripts/run.sh"] {
            assert!(valid_preset_path(path), "{path}");
        }
        for path in [
            "SKILL.md",
            "skills/SKILL.md",
            "skills/../x/y",
            "skills//x",
            "/skills/a/b",
            "skills/a b/c",
            "agents/a/b",
        ] {
            assert!(!valid_preset_path(path), "{path}");
        }
    }

    #[test]
    fn credentials_do_not_show_in_debug_output() {
        let credentials = ModelCredentials {
            sections: [("anthropic".to_owned(), json!({"access_token": "sk-secret"}))]
                .into_iter()
                .collect(),
            accounts: BTreeMap::new(),
            valid_secs: Some(60),
            errors: Vec::new(),
        };
        assert!(!format!("{credentials:?}").contains("sk-secret"));
    }

    #[test]
    fn login_prompts_tolerate_new_kinds() {
        let prompt: ModelLoginPrompt =
            serde_json::from_value(json!({"type": "qr_code", "data": "x"})).unwrap();
        assert_eq!(prompt, ModelLoginPrompt::Unknown);
        assert_eq!(AccountKind::parse("api_key"), AccountKind::ApiKey);
        let method: ModelLoginMethod = serde_json::from_value(json!({"type": "browser"})).unwrap();
        assert_eq!(method, ModelLoginMethod::Browser { redirect_uri: None });
    }
}
