pub mod callback;
pub mod codex;
pub mod jwt;
pub mod pkce;
pub mod siwc;
pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{LlmError, Result};
use crate::provider::ProviderId;
pub use callback::CallbackParams;
use codex::{CodexClient, CodexPending, DeviceAuth};
use siwc::{SiwcClient, SiwcPending};
pub use store::{
    ChatGptAccount, CodexCredential, CredentialStore, Credentials, OAuthTokens, OpenAiCredentials,
};

pub const REFRESH_MARGIN_SECS: u64 = 300;
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
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginStarted {
    Browser {
        login_id: String,
        url: String,
        redirect_uri: String,
    },
    DeviceCode {
        login_id: String,
        user_code: String,
        verification_uri: String,
        expires_in_secs: u64,
    },
}

impl LoginStarted {
    pub fn login_id(&self) -> &str {
        match self {
            Self::Browser { login_id, .. } | Self::DeviceCode { login_id, .. } => login_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginInput {
    Callback(CallbackParams),
    PastedRedirect { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub provider: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    pub openai_api_key: bool,
    pub openai_accounts: Vec<ChatGptAccountStatus>,
    pub codex_account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatGptAccountStatus {
    pub client_id: String,
    pub email: Option<String>,
    pub active: bool,
    pub signed_in: bool,
}

enum Pending {
    ChatGpt(SiwcPending),
    CodexBrowser(CodexPending),
    CodexDevice(DeviceAuth),
}

#[derive(Clone)]
pub struct Auth {
    store: CredentialStore,
    siwc: SiwcClient,
    codex: CodexClient,
    pending: Arc<Mutex<HashMap<String, (Instant, Pending)>>>,
}

fn not_signed_in(provider: ProviderId) -> LlmError {
    LlmError::NotAuthenticated {
        provider: provider.as_str().to_owned(),
        message: "sign in first".into(),
    }
}

impl Auth {
    pub fn new(store: CredentialStore, http: reqwest::Client) -> Self {
        Self::with_clients(store, SiwcClient::new(http.clone()), CodexClient::new(http))
    }

    pub fn with_clients(store: CredentialStore, siwc: SiwcClient, codex: CodexClient) -> Self {
        Self {
            store,
            siwc,
            codex,
            pending: Arc::default(),
        }
    }

    pub fn store(&self) -> &CredentialStore {
        &self.store
    }

    pub async fn start_login(
        &self,
        provider: ProviderId,
        method: LoginMethod,
    ) -> Result<LoginStarted> {
        let login_id = uuid::Uuid::new_v4().to_string();
        let (started, pending) = match (provider, method) {
            (
                ProviderId::OpenAi,
                LoginMethod::Browser {
                    redirect_uri,
                    account,
                },
            ) => {
                let redirect_uri =
                    redirect_uri.unwrap_or_else(|| siwc::loopback_redirect_uri(siwc::DEFAULT_PORT));
                let host_id = self.store.host_id().await?;
                let creds = self.store.load().await?;
                let account = match account {
                    Some(client_id) => Some(
                        creds
                            .openai
                            .accounts
                            .iter()
                            .find(|a| a.client_id == client_id)
                            .cloned()
                            .ok_or_else(|| {
                                LlmError::login(format!("unknown ChatGPT account {client_id}"))
                            })?,
                    ),
                    None => None,
                };
                let pending = self.siwc.start(&redirect_uri, &host_id, account.as_ref())?;
                (
                    LoginStarted::Browser {
                        login_id: login_id.clone(),
                        url: pending.url.clone(),
                        redirect_uri,
                    },
                    Pending::ChatGpt(pending),
                )
            }
            (ProviderId::OpenAi, LoginMethod::DeviceCode) => {
                return Err(LlmError::login(
                    "Sign in with ChatGPT does not support device code login",
                ));
            }
            (ProviderId::OpenAiCodex, LoginMethod::Browser { .. }) => {
                let pending = self.codex.start_browser()?;
                (
                    LoginStarted::Browser {
                        login_id: login_id.clone(),
                        url: pending.url.clone(),
                        redirect_uri: codex::REDIRECT_URI.to_owned(),
                    },
                    Pending::CodexBrowser(pending),
                )
            }
            (ProviderId::OpenAiCodex, LoginMethod::DeviceCode) => {
                let device = self.codex.start_device().await?;
                (
                    LoginStarted::DeviceCode {
                        login_id: login_id.clone(),
                        user_code: device.user_code.clone(),
                        verification_uri: device.verification_uri.clone(),
                        expires_in_secs: codex::DEVICE_TIMEOUT.as_secs(),
                    },
                    Pending::CodexDevice(device),
                )
            }
        };
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, (created, _)| created.elapsed() < PENDING_TTL);
        map.insert(login_id, (Instant::now(), pending));
        Ok(started)
    }

    fn take_pending(&self, login_id: &str) -> Result<Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(login_id)
            .map(|(_, p)| p)
            .ok_or_else(|| LlmError::login("unknown or expired login"))
    }

    pub fn cancel_login(&self, login_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(login_id);
    }

    pub async fn complete_login(&self, login_id: &str, input: LoginInput) -> Result<AccountInfo> {
        match self.take_pending(login_id)? {
            Pending::ChatGpt(pending) => {
                let params = match input {
                    LoginInput::Callback(params) => params,
                    LoginInput::PastedRedirect { url } => {
                        CallbackParams::from_redirect_url(&url, &pending.redirect_uri)?
                    }
                };
                let account = self.siwc.complete(&pending, &params).await?;
                let info = AccountInfo {
                    provider: ProviderId::OpenAi,
                    email: account.email.clone(),
                    account_id: Some(account.client_id.clone()),
                };
                self.store
                    .update(move |creds| creds.openai.upsert(account))
                    .await?;
                Ok(info)
            }
            Pending::CodexBrowser(pending) => {
                let credential = match input {
                    LoginInput::Callback(params) => {
                        self.codex.complete_browser(&pending, &params).await?
                    }
                    LoginInput::PastedRedirect { url } => {
                        self.codex.complete_manual(&pending, &url).await?
                    }
                };
                self.save_codex(credential).await
            }
            Pending::CodexDevice(_) => Err(LlmError::login(
                "device code logins complete by polling, not with callback input",
            )),
        }
    }

    pub async fn wait_device_login(
        &self,
        login_id: &str,
        cancel: &CancellationToken,
    ) -> Result<AccountInfo> {
        let Pending::CodexDevice(device) = self.take_pending(login_id)? else {
            return Err(LlmError::login("not a device code login"));
        };
        let credential = self.codex.poll_device(&device, cancel).await?;
        self.save_codex(credential).await
    }

    async fn save_codex(&self, credential: CodexCredential) -> Result<AccountInfo> {
        let info = AccountInfo {
            provider: ProviderId::OpenAiCodex,
            email: None,
            account_id: Some(credential.account_id.clone()),
        };
        self.store
            .update(move |creds| creds.openai_codex = Some(credential))
            .await?;
        Ok(info)
    }

    pub async fn set_api_key(&self, key: Option<String>) -> Result<()> {
        let key = key.filter(|k| !k.trim().is_empty());
        self.store
            .update(move |creds| creds.openai.api_key = key)
            .await
    }

    pub async fn select_account(&self, client_id: &str) -> Result<()> {
        let client_id = client_id.to_owned();
        self.store
            .update(move |creds| {
                if creds.openai.account_mut(&client_id).is_none() {
                    return Err(LlmError::login(format!(
                        "unknown ChatGPT account {client_id}"
                    )));
                }
                creds.openai.active = Some(client_id);
                Ok(())
            })
            .await?
    }

    pub async fn logout(&self, provider: ProviderId) -> Result<()> {
        match provider {
            ProviderId::OpenAi => {
                let creds = self.store.load().await?;
                let Some(account) = creds.openai.active_account().cloned() else {
                    return Ok(());
                };
                if let Err(err) = self.siwc.revoke(&account).await {
                    tracing::warn!(error = %err, "could not revoke ChatGPT session");
                }
                self.store
                    .update(move |creds| {
                        if let Some(stored) = creds.openai.account_mut(&account.client_id) {
                            stored.tokens = None;
                            stored.id_token = None;
                        }
                    })
                    .await
            }
            ProviderId::OpenAiCodex => self.store.update(|creds| creds.openai_codex = None).await,
        }
    }

    pub async fn status(&self) -> Result<AuthStatus> {
        let creds = self.store.load().await?;
        let active = creds.openai.active.clone();
        Ok(AuthStatus {
            openai_api_key: creds.openai.api_key.is_some(),
            openai_accounts: creds
                .openai
                .accounts
                .iter()
                .map(|a| ChatGptAccountStatus {
                    client_id: a.client_id.clone(),
                    email: a.email.clone(),
                    active: active.as_deref() == Some(a.client_id.as_str()),
                    signed_in: a.tokens.is_some(),
                })
                .collect(),
            codex_account: creds.openai_codex.map(|c| c.account_id),
        })
    }

    pub async fn stored_api_key(&self) -> Result<Option<String>> {
        Ok(self.store.load().await?.openai.api_key)
    }

    pub async fn chatgpt_access_token(&self) -> Result<String> {
        let creds = self.store.load().await?;
        let account = creds
            .openai
            .active_account()
            .ok_or_else(|| not_signed_in(ProviderId::OpenAi))?;
        let tokens = account
            .tokens
            .as_ref()
            .ok_or_else(|| not_signed_in(ProviderId::OpenAi))?;
        if tokens.is_fresh(REFRESH_MARGIN_SECS) {
            return Ok(tokens.access_token.clone());
        }
        let client_id = account.client_id.clone();
        let guard = self.store.lock().await?;
        let mut creds = guard.load().await?;
        let account = creds
            .openai
            .account_mut(&client_id)
            .ok_or_else(|| not_signed_in(ProviderId::OpenAi))?;
        if let Some(tokens) = &account.tokens
            && tokens.is_fresh(REFRESH_MARGIN_SECS)
        {
            return Ok(tokens.access_token.clone());
        }
        let tokens = self.siwc.refresh(account).await?;
        let access = tokens.access_token.clone();
        account.tokens = Some(tokens);
        guard.save(creds).await?;
        Ok(access)
    }

    pub async fn codex_access_token(&self) -> Result<(String, String)> {
        let creds = self.store.load().await?;
        let credential = creds
            .openai_codex
            .ok_or_else(|| not_signed_in(ProviderId::OpenAiCodex))?;
        if credential.is_fresh(REFRESH_MARGIN_SECS) {
            return Ok((credential.access_token, credential.account_id));
        }
        let guard = self.store.lock().await?;
        let mut creds = guard.load().await?;
        let credential = creds
            .openai_codex
            .as_ref()
            .ok_or_else(|| not_signed_in(ProviderId::OpenAiCodex))?;
        if credential.is_fresh(REFRESH_MARGIN_SECS) {
            return Ok((
                credential.access_token.clone(),
                credential.account_id.clone(),
            ));
        }
        let refreshed = self.codex.refresh(credential).await?;
        let result = (refreshed.access_token.clone(), refreshed.account_id.clone());
        creds.openai_codex = Some(refreshed);
        guard.save(creds).await?;
        Ok(result)
    }
}
