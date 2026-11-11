mod auth;
mod inference;
mod websocket;

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::auth::{
    AccountInfo, AccountStatus, LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt,
    ProviderStatus, RedirectSpec,
};
use crate::error::{LlmError, Result};
use crate::kind::{
    PendingLogin, ProviderKind, Shared, StartedLogin, check_http_url, parse_settings,
};
use crate::provider::{LlmProvider, ProviderId};
use crate::store::CredentialStore;
pub(crate) use auth::ORIGINATOR;
use auth::{
    CALLBACK_PORT, CodexClient, CodexCredential, CodexEndpoints, CodexPending, DEVICE_TIMEOUT,
    DeviceAuth, REDIRECT_URI,
};

pub(crate) const ID: ProviderId = ProviderId::from_static("openai-codex");
const DEFAULT_AUTH_BASE: &str = "https://auth.openai.com";
const REFRESH_MARGIN_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    #[default]
    CodexOauth,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    #[allow(dead_code)]
    pub auth: AuthMode,
    pub base_url: Option<String>,
    /// Sends responses over a WebSocket, falling back to HTTP. On by default.
    pub websockets: Option<bool>,
}

impl Settings {
    fn check(&self) -> Result<(), String> {
        match &self.base_url {
            Some(url) => check_http_url(url).map_err(|e| format!("base_url: {e}")),
            None => Ok(()),
        }
    }
}

fn client(shared: &Shared) -> CodexClient {
    CodexClient::with_endpoints(
        shared.http.clone(),
        CodexEndpoints {
            base: shared.auth_base_url(&ID, DEFAULT_AUTH_BASE),
        },
    )
}

fn not_signed_in() -> LlmError {
    LlmError::NotAuthenticated {
        provider: ID.to_string(),
        message: "sign in to Codex first".into(),
    }
}

/// Whether Codex credentials are stored, fresh or not: a stale token is
/// refreshed when it is used.
pub(crate) async fn signed_in(shared: &Shared) -> Result<bool> {
    Ok(shared
        .store
        .load_section::<CodexCredential>(ID.as_str())
        .await?
        .is_some())
}

pub(crate) async fn access_token(shared: &Shared) -> Result<(String, String)> {
    let credential = shared
        .store
        .load_section::<CodexCredential>(ID.as_str())
        .await?
        .ok_or_else(not_signed_in)?;
    if credential.is_fresh(REFRESH_MARGIN_SECS) {
        return Ok((credential.access_token, credential.account_id));
    }
    let guard = shared.store.lock().await?;
    let credential = guard
        .load_section::<CodexCredential>(ID.as_str())
        .await?
        .ok_or_else(not_signed_in)?;
    if credential.is_fresh(REFRESH_MARGIN_SECS) {
        return Ok((credential.access_token, credential.account_id));
    }
    let refreshed = client(shared).refresh(&credential).await?;
    guard.save_section(ID.as_str(), Some(&refreshed)).await?;
    Ok((refreshed.access_token, refreshed.account_id))
}

async fn save(store: &CredentialStore, credential: CodexCredential) -> Result<AccountInfo> {
    let info = AccountInfo {
        provider: ID,
        account_id: Some(credential.account_id.clone()),
        label: None,
    };
    store
        .update_section::<CodexCredential, _, _>(ID.as_str(), move |section| {
            *section = Some(credential)
        })
        .await?;
    Ok(info)
}

struct BrowserLogin {
    client: CodexClient,
    pending: CodexPending,
    store: CredentialStore,
}

#[async_trait]
impl PendingLogin for BrowserLogin {
    async fn complete(&self, input: LoginInput) -> Result<AccountInfo> {
        let text = match input {
            LoginInput::Redirect { url } => url,
            LoginInput::Code { code } => code,
        };
        let credential = self.client.complete_manual(&self.pending, &text).await?;
        save(&self.store, credential).await
    }
}

struct DeviceLogin {
    client: CodexClient,
    device: DeviceAuth,
    store: CredentialStore,
}

#[async_trait]
impl PendingLogin for DeviceLogin {
    async fn wait(&self, cancel: &CancellationToken) -> Result<AccountInfo> {
        let credential = self.client.poll_device(&self.device, cancel).await?;
        save(&self.store, credential).await
    }
}

pub(crate) struct Kind;

#[async_trait]
impl ProviderKind for Kind {
    fn id(&self) -> ProviderId {
        ID
    }

    fn display_name(&self) -> String {
        "OpenAI Codex (legacy)".into()
    }

    fn login_methods(&self) -> Vec<LoginMethodInfo> {
        vec![
            LoginMethodInfo::Browser {
                redirect: RedirectSpec {
                    host: "localhost".into(),
                    port: Some(CALLBACK_PORT),
                    path: "/auth/callback".into(),
                },
            },
            LoginMethodInfo::DeviceCode,
        ]
    }

    fn validate(&self, settings: &Value) -> Result<(), String> {
        parse_settings::<Settings>(Some(settings))?.check()
    }

    fn build(&self, settings: Option<&Value>, shared: &Shared) -> Result<Arc<dyn LlmProvider>> {
        let settings = parse_settings::<Settings>(settings)
            .and_then(|s| s.check().map(|()| s))
            .map_err(|message| LlmError::Config {
                field: format!("llm.providers.{ID}"),
                message,
            })?;
        Ok(Arc::new(inference::CodexProvider::new(
            shared.clone(),
            settings,
        )))
    }

    async fn start_login(&self, shared: &Shared, method: LoginMethod) -> Result<StartedLogin> {
        let client = client(shared);
        match method {
            LoginMethod::Browser { .. } => {
                let pending = client.start_browser()?;
                Ok(StartedLogin {
                    prompt: LoginPrompt::OpenUrl {
                        url: pending.url.clone(),
                        redirect_uri: REDIRECT_URI.into(),
                    },
                    pending: Arc::new(BrowserLogin {
                        client,
                        pending,
                        store: shared.store.clone(),
                    }),
                })
            }
            LoginMethod::DeviceCode => {
                let device = client.start_device().await?;
                Ok(StartedLogin {
                    prompt: LoginPrompt::DeviceCode {
                        user_code: device.user_code.clone(),
                        verification_uri: device.verification_uri.clone(),
                        expires_in_secs: DEVICE_TIMEOUT.as_secs(),
                    },
                    pending: Arc::new(DeviceLogin {
                        client,
                        device,
                        store: shared.store.clone(),
                    }),
                })
            }
        }
    }

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus> {
        let credential = shared
            .store
            .load_section::<CodexCredential>(ID.as_str())
            .await?;
        Ok(ProviderStatus {
            provider: ID,
            display_name: self.display_name(),
            login_methods: self.login_methods(),
            api_key_set: false,
            accounts: credential
                .into_iter()
                .map(|c| AccountStatus {
                    id: c.account_id,
                    label: None,
                    active: true,
                    signed_in: true,
                })
                .collect(),
        })
    }

    async fn logout(&self, shared: &Shared) -> Result<()> {
        shared
            .store
            .update_section::<CodexCredential, _, _>(ID.as_str(), |section| *section = None)
            .await
    }
}
