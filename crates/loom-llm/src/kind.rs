use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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

#[derive(Clone, Default)]
pub(crate) struct CustomKinds(Arc<RwLock<Vec<Arc<dyn ProviderKind>>>>);

impl std::fmt::Debug for CustomKinds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids: Vec<String> = self.list().iter().map(|k| k.id().to_string()).collect();
        f.debug_tuple("CustomKinds").field(&ids).finish()
    }
}

impl CustomKinds {
    pub fn list(&self) -> Vec<Arc<dyn ProviderKind>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace(&self, kinds: Vec<Arc<dyn ProviderKind>>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = kinds;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Shared {
    pub store: CredentialStore,
    pub http: HttpClient,
    pub auth_base_urls: HashMap<String, String>,
    pub retry: RetryPolicy,
    pub custom: CustomKinds,
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

    fn display_name(&self) -> String;

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

pub(crate) fn builtin_kind(id: &str) -> Option<Arc<dyn ProviderKind>> {
    providers::builtin()
        .into_iter()
        .find(|k| k.id().as_str() == id)
}

pub(crate) fn kinds(shared: &Shared) -> Vec<Arc<dyn ProviderKind>> {
    let mut all = providers::builtin();
    all.extend(shared.custom.list());
    all
}

pub(crate) fn require_kind(shared: &Shared, id: &ProviderId) -> Result<Arc<dyn ProviderKind>> {
    kinds(shared)
        .into_iter()
        .find(|k| &k.id() == id)
        .ok_or_else(|| LlmError::UnknownProvider(id.to_string()))
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
