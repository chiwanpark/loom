use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::HttpClient;
use crate::auth::{
    AccountInfo, LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt, ProviderStatus,
};
use crate::error::{LlmError, Result};
use crate::provider::{LlmProvider, ProviderId};
use crate::providers;
use crate::retry::RetryPolicy;
use crate::store::CredentialStore;

#[derive(Debug, Clone)]
pub(crate) struct Shared {
    pub store: CredentialStore,
    pub http: HttpClient,
    pub auth_base_urls: HashMap<String, String>,
    pub retry: RetryPolicy,
}

impl Shared {
    pub fn auth_base_url(&self, provider: &ProviderId, default: &str) -> String {
        self.auth_base_urls
            .get(provider.as_str())
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    }
}

#[async_trait]
pub(crate) trait PendingLogin: Send + Sync {
    async fn complete(&self, _input: LoginInput) -> Result<AccountInfo> {
        Err(LlmError::login(
            "this login completes by itself; wait for it instead",
        ))
    }

    async fn wait(&self, _cancel: &CancellationToken) -> Result<AccountInfo> {
        Err(LlmError::login(
            "this login needs input from the browser redirect",
        ))
    }
}

pub(crate) struct StartedLogin {
    pub prompt: LoginPrompt,
    pub pending: Arc<dyn PendingLogin>,
}

#[async_trait]
pub(crate) trait ProviderKind: Send + Sync {
    fn id(&self) -> ProviderId;

    fn display_name(&self) -> &'static str;

    fn login_methods(&self) -> Vec<LoginMethodInfo>;

    fn validate(&self, settings: &Value) -> Result<(), String>;

    fn build(&self, settings: Option<&Value>, shared: &Shared) -> Result<Arc<dyn LlmProvider>>;

    async fn start_login(&self, shared: &Shared, method: LoginMethod) -> Result<StartedLogin>;

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus>;

    async fn logout(&self, shared: &Shared) -> Result<()>;

    async fn set_api_key(&self, _shared: &Shared, _key: Option<String>) -> Result<()> {
        Err(LlmError::Unsupported(format!(
            "{} does not use API keys",
            self.id()
        )))
    }

    async fn select_account(&self, _shared: &Shared, _account: &str) -> Result<()> {
        Err(LlmError::Unsupported(format!(
            "{} has a single account",
            self.id()
        )))
    }
}

pub(crate) fn kinds() -> &'static [&'static dyn ProviderKind] {
    providers::KINDS
}

pub(crate) fn kind(id: &str) -> Option<&'static dyn ProviderKind> {
    kinds().iter().copied().find(|k| k.id().as_str() == id)
}

pub(crate) fn require_kind(id: &ProviderId) -> Result<&'static dyn ProviderKind> {
    kind(id.as_str()).ok_or_else(|| LlmError::UnknownProvider(id.to_string()))
}

pub(crate) fn parse_settings<T: serde::de::DeserializeOwned + Default>(
    settings: Option<&Value>,
) -> Result<T, String> {
    match settings {
        None => Ok(T::default()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|e| e.to_string()),
    }
}

pub(crate) fn check_http_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(format!("{url:?} must start with https:// or http://"))
    }
}
