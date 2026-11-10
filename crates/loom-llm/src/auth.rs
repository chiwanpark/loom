use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::HttpClient;
use crate::error::{LlmError, Result};
use crate::kind::{PendingLogin, Shared, kinds, require_kind};
use crate::provider::ProviderId;
use crate::retry::RetryPolicy;
use crate::store::CredentialStore;

const PENDING_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginMethod {
    Browser {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redirect_uri: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginMethodInfo {
    Browser { redirect: RedirectSpec },
    DeviceCode,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    pub login_id: String,
    pub prompt: LoginPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginInput {
    Redirect { url: String },
    Code { code: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub provider: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountStatus {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub active: bool,
    pub signed_in: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub provider: ProviderId,
    pub display_name: String,
    pub login_methods: Vec<LoginMethodInfo>,
    pub api_key_set: bool,
    pub accounts: Vec<AccountStatus>,
}

impl ProviderStatus {
    pub fn signed_in(&self) -> bool {
        self.api_key_set || self.accounts.iter().any(|a| a.active && a.signed_in)
    }
}

struct Pending {
    provider: ProviderId,
    created: Instant,
    login: Arc<dyn PendingLogin>,
}

#[derive(Clone)]
pub struct Auth {
    shared: Arc<Shared>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
}

impl Auth {
    pub fn new(store: CredentialStore, http: HttpClient) -> Self {
        Self {
            shared: Arc::new(Shared {
                store,
                http,
                auth_base_urls: HashMap::new(),
                retry: RetryPolicy::default(),
            }),
            pending: Arc::default(),
        }
    }

    pub fn with_auth_base_url(
        mut self,
        provider: impl Into<ProviderId>,
        url: impl Into<String>,
    ) -> Self {
        Arc::make_mut(&mut self.shared)
            .auth_base_urls
            .insert(provider.into().to_string(), url.into());
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        Arc::make_mut(&mut self.shared).retry = retry;
        self
    }

    pub(crate) fn shared(&self) -> &Shared {
        &self.shared
    }

    pub fn store(&self) -> &CredentialStore {
        &self.shared.store
    }

    pub fn providers(&self) -> Vec<ProviderId> {
        kinds().iter().map(|k| k.id()).collect()
    }

    pub async fn start_login(
        &self,
        provider: &ProviderId,
        method: LoginMethod,
    ) -> Result<LoginStarted> {
        let started = require_kind(provider)?
            .start_login(&self.shared, method)
            .await?;
        let login_id = uuid::Uuid::new_v4().to_string();
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
        pending.insert(
            login_id.clone(),
            Pending {
                provider: provider.clone(),
                created: Instant::now(),
                login: started.pending,
            },
        );
        Ok(LoginStarted {
            login_id,
            prompt: started.prompt,
        })
    }

    fn take(&self, login_id: &str) -> Result<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(login_id)
            .ok_or_else(|| LlmError::login("unknown or expired login"))
    }

    pub fn login_provider(&self, login_id: &str) -> Option<ProviderId> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(login_id)
            .map(|p| p.provider.clone())
    }

    pub async fn complete_login(&self, login_id: &str, input: LoginInput) -> Result<AccountInfo> {
        self.take(login_id)?.login.complete(input).await
    }

    pub async fn wait_login(
        &self,
        login_id: &str,
        cancel: &CancellationToken,
    ) -> Result<AccountInfo> {
        self.take(login_id)?.login.wait(cancel).await
    }

    pub fn cancel_login(&self, login_id: &str) -> bool {
        self.take(login_id).is_ok()
    }

    pub async fn set_api_key(&self, provider: &ProviderId, key: Option<String>) -> Result<()> {
        let key = key.filter(|k| !k.trim().is_empty());
        require_kind(provider)?.set_api_key(&self.shared, key).await
    }

    pub async fn select_account(&self, provider: &ProviderId, account: &str) -> Result<()> {
        require_kind(provider)?
            .select_account(&self.shared, account)
            .await
    }

    pub async fn logout(&self, provider: &ProviderId) -> Result<()> {
        require_kind(provider)?.logout(&self.shared).await
    }

    pub async fn provider_status(&self, provider: &ProviderId) -> Result<ProviderStatus> {
        require_kind(provider)?.status(&self.shared).await
    }

    pub async fn status(&self) -> Result<Vec<ProviderStatus>> {
        let mut all = Vec::new();
        for kind in kinds() {
            all.push(kind.status(&self.shared).await?);
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_providers_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(CredentialStore::in_dir(dir.path()), HttpClient::new());
        let err = auth
            .start_login(&ProviderId::new("nope"), LoginMethod::DeviceCode)
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::UnknownProvider(_)));
        assert!(
            auth.complete_login("x", LoginInput::Code { code: "c".into() })
                .await
                .is_err()
        );
        assert!(!auth.cancel_login("x"));
    }

    #[tokio::test]
    async fn status_lists_every_provider() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(CredentialStore::in_dir(dir.path()), HttpClient::new());
        let status = auth.status().await.unwrap();
        assert_eq!(
            status
                .iter()
                .map(|s| s.provider.clone())
                .collect::<Vec<_>>(),
            auth.providers()
        );
        assert!(
            status
                .iter()
                .all(|s| !s.signed_in() && !s.login_methods.is_empty())
        );
    }
}
