use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
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

/// Who the signed-in account of a provider is: what telemetry reports as
/// `user.email`, `user.account_uuid`, and `organization.id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountIdentity {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub organization_uuid: Option<String>,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LentCredential {
    pub section: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for LentCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LentCredential")
            .field("section", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl LentCredential {
    pub(crate) fn new<T: Serialize>(section: &T, expires_at: Option<u64>) -> Result<Self> {
        let section = serde_json::to_value(section)
            .map_err(|e| LlmError::Unsupported(format!("cannot lend a credential: {e}")))?;
        Ok(Self::from_section(section, expires_at))
    }

    pub(crate) fn from_section(section: Value, expires_at: Option<u64>) -> Self {
        Self {
            section,
            expires_at,
        }
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
                custom: Default::default(),
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
        kinds(&self.shared).iter().map(|k| k.id()).collect()
    }

    fn writable(&self) -> Result<()> {
        match self.shared.store.read_only() {
            Some(reason) => Err(LlmError::Unsupported(reason)),
            None => Ok(()),
        }
    }

    pub async fn start_login(
        &self,
        provider: &ProviderId,
        method: LoginMethod,
    ) -> Result<LoginStarted> {
        self.writable()?;
        let started = require_kind(&self.shared, provider)?
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
        self.writable()?;
        let key = key.filter(|k| !k.trim().is_empty());
        require_kind(&self.shared, provider)?
            .set_api_key(&self.shared, key)
            .await
    }

    pub async fn select_account(&self, provider: &ProviderId, account: &str) -> Result<()> {
        self.writable()?;
        require_kind(&self.shared, provider)?
            .select_account(&self.shared, account)
            .await
    }

    pub async fn logout(&self, provider: &ProviderId) -> Result<()> {
        self.writable()?;
        require_kind(&self.shared, provider)?
            .logout(&self.shared)
            .await
    }

    pub async fn provider_status(&self, provider: &ProviderId) -> Result<ProviderStatus> {
        let status = require_kind(&self.shared, provider)?
            .status(&self.shared)
            .await?;
        let names = self.shared.store.account_names().await;
        Ok(self.shown(status, &names))
    }

    /// Who the signed-in account of `provider` is, from the stored sign-in
    /// only. `None` when the provider has no account or does not tell.
    pub async fn account_identity(&self, provider: &ProviderId) -> Result<Option<AccountIdentity>> {
        require_kind(&self.shared, provider)?
            .account_identity(&self.shared)
            .await
    }

    pub async fn status(&self) -> Result<Vec<ProviderStatus>> {
        let mut all = Vec::new();
        let names = self.shared.store.account_names().await;
        for kind in kinds(&self.shared) {
            let status = kind.status(&self.shared).await?;
            all.push(self.shown(status, &names));
        }
        Ok(all)
    }

    fn shown(
        &self,
        mut status: ProviderStatus,
        names: &std::collections::BTreeMap<String, String>,
    ) -> ProviderStatus {
        if self.shared.store.read_only().is_some() {
            status.login_methods.clear();
        }
        if let Some(name) = names.get(status.provider.as_str()) {
            for account in status.accounts.iter_mut().filter(|a| a.active) {
                account.label = Some(match account.label.take() {
                    Some(label) => format!("{name} ({label})"),
                    None => name.clone(),
                });
            }
            if status.accounts.is_empty() && status.api_key_set {
                status.accounts.push(AccountStatus {
                    id: name.clone(),
                    label: Some(name.clone()),
                    active: true,
                    signed_in: true,
                });
            }
        }
        status
    }

    pub async fn lend(
        &self,
        provider: &ProviderId,
        margin_secs: u64,
    ) -> Result<Option<LentCredential>> {
        require_kind(&self.shared, provider)?
            .lend(&self.shared, margin_secs)
            .await
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
