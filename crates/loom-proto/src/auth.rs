use serde::{Deserialize, Serialize};

pub type ProviderId = String;
pub type LoginId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginMethod {
    Browser {
        redirect_uri: Option<String>,
        account: Option<String>,
    },
    DeviceCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectSpec {
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

impl RedirectSpec {
    pub fn uri(&self, port: u16) -> String {
        format!("http://{}:{port}{}", self.host, self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginMethodInfo {
    Browser { redirect: RedirectSpec },
    DeviceCode,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginPrompt {
    OpenUrl {
        url: String,
        redirect_uri: String,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        expires_in_secs: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginStarted {
    pub login_id: LoginId,
    pub prompt: LoginPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginInput {
    Redirect { url: String },
    Code { code: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub provider: ProviderId,
    pub account_id: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStatus {
    pub id: String,
    pub label: Option<String>,
    pub active: bool,
    pub signed_in: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub provider: ProviderId,
    pub display_name: String,
    pub enabled: bool,
    pub login_methods: Vec<LoginMethodInfo>,
    pub api_key_set: bool,
    pub accounts: Vec<AccountStatus>,
}

impl ProviderStatus {
    pub fn signed_in(&self) -> bool {
        self.api_key_set || self.accounts.iter().any(|a| a.active && a.signed_in)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: ProviderId,
    pub id: String,
    pub display_name: String,
}

impl ModelInfo {
    pub fn model_ref(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}
