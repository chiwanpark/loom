mod auth;
mod inference;

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::{
    AccountInfo, AccountStatus, LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt,
    ProviderStatus, RedirectSpec,
};
use crate::error::{LlmError, Result};
use crate::kind::{
    PendingLogin, ProviderKind, Shared, StartedLogin, check_http_url, parse_settings,
};
use crate::oauth::callback::CallbackParams;
use crate::provider::{LlmProvider, ProviderId};
use crate::store::CredentialStore;
use auth::{
    CALLBACK_PATH, DEFAULT_PORT, OpenAiCredentials, SiwcClient, SiwcEndpoints, SiwcPending,
};

pub(crate) const ID: ProviderId = ProviderId::from_static("openai");
const DEFAULT_AUTH_BASE: &str = "https://auth.openai.com";
const REFRESH_MARGIN_SECS: u64 = 300;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    #[default]
    Chatgpt,
    ApiKey,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub auth: AuthMode,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
}

impl Settings {
    fn check(&self) -> Result<(), String> {
        if let Some(url) = &self.base_url {
            check_http_url(url).map_err(|e| format!("base_url: {e}"))?;
        }
        if self
            .api_key_env
            .as_deref()
            .is_some_and(|env| env.trim().is_empty())
        {
            return Err("api_key_env must not be empty".into());
        }
        Ok(())
    }
}

fn client(shared: &Shared) -> SiwcClient {
    SiwcClient::with_endpoints(
        shared.http.clone(),
        SiwcEndpoints::for_issuer(&shared.auth_base_url(&ID, DEFAULT_AUTH_BASE)),
    )
}

fn not_signed_in() -> LlmError {
    LlmError::NotAuthenticated {
        provider: ID.to_string(),
        message: "sign in with ChatGPT first".into(),
    }
}

async fn load(store: &CredentialStore) -> Result<OpenAiCredentials> {
    Ok(store
        .load_section::<OpenAiCredentials>(ID.as_str())
        .await?
        .unwrap_or_default())
}

async fn host_id(store: &CredentialStore) -> Result<String> {
    if let Some(id) = load(store).await?.host_id {
        return Ok(id);
    }
    store
        .update_section::<OpenAiCredentials, _, _>(ID.as_str(), |creds| {
            creds
                .get_or_insert_default()
                .host_id
                .get_or_insert_with(|| format!("urn:uuid:{}", uuid::Uuid::new_v4()))
                .clone()
        })
        .await
}

pub(crate) async fn stored_api_key(shared: &Shared) -> Result<Option<String>> {
    Ok(load(&shared.store).await?.api_key)
}

pub(crate) async fn chatgpt_access_token(shared: &Shared) -> Result<String> {
    let creds = load(&shared.store).await?;
    let account = creds.active_account().ok_or_else(not_signed_in)?;
    let tokens = account.tokens.as_ref().ok_or_else(not_signed_in)?;
    if tokens.is_fresh(REFRESH_MARGIN_SECS) {
        return Ok(tokens.access_token.clone());
    }
    let client_id = account.client_id.clone();
    let guard = shared.store.lock().await?;
    let mut creds: OpenAiCredentials = guard.load_section(ID.as_str()).await?.unwrap_or_default();
    let account = creds.account_mut(&client_id).ok_or_else(not_signed_in)?;
    if let Some(tokens) = &account.tokens
        && tokens.is_fresh(REFRESH_MARGIN_SECS)
    {
        return Ok(tokens.access_token.clone());
    }
    let tokens = client(shared).refresh(account).await?;
    let access = tokens.access_token.clone();
    account.tokens = Some(tokens);
    guard.save_section(ID.as_str(), Some(&creds)).await?;
    Ok(access)
}

struct BrowserLogin {
    client: SiwcClient,
    pending: SiwcPending,
    store: CredentialStore,
}

#[async_trait]
impl PendingLogin for BrowserLogin {
    async fn complete(&self, input: LoginInput) -> Result<AccountInfo> {
        let params = match input {
            LoginInput::Redirect { url } => {
                CallbackParams::from_redirect_url(&url, &self.pending.redirect_uri)?
            }
            LoginInput::Code { .. } => {
                return Err(LlmError::login(
                    "Sign in with ChatGPT needs the full redirect URL",
                ));
            }
        };
        let account = self.client.complete(&self.pending, &params).await?;
        let info = AccountInfo {
            provider: ID,
            account_id: Some(account.client_id.clone()),
            label: account.email.clone(),
        };
        self.store
            .update_section::<OpenAiCredentials, _, _>(ID.as_str(), move |creds| {
                creds.get_or_insert_default().upsert(account)
            })
            .await?;
        Ok(info)
    }
}

pub(crate) struct Kind;

#[async_trait]
impl ProviderKind for Kind {
    fn id(&self) -> ProviderId {
        ID
    }

    fn display_name(&self) -> String {
        "OpenAI".into()
    }

    fn login_methods(&self) -> Vec<LoginMethodInfo> {
        vec![
            LoginMethodInfo::Browser {
                redirect: RedirectSpec {
                    host: "127.0.0.1".into(),
                    port: None,
                    path: CALLBACK_PATH.into(),
                },
            },
            LoginMethodInfo::ApiKey,
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
        Ok(Arc::new(inference::OpenAiProvider::new(
            shared.clone(),
            settings,
        )))
    }

    async fn start_login(&self, shared: &Shared, method: LoginMethod) -> Result<StartedLogin> {
        let LoginMethod::Browser {
            redirect_uri,
            account,
        } = method
        else {
            return Err(LlmError::Unsupported(
                "Sign in with ChatGPT does not support device code login".into(),
            ));
        };
        let redirect_uri =
            redirect_uri.unwrap_or_else(|| auth::loopback_redirect_uri(DEFAULT_PORT));
        let host_id = host_id(&shared.store).await?;
        let account = match account {
            Some(client_id) => Some(
                load(&shared.store)
                    .await?
                    .accounts
                    .into_iter()
                    .find(|a| a.client_id == client_id)
                    .ok_or_else(|| LlmError::login(format!("unknown account {client_id}")))?,
            ),
            None => None,
        };
        let client = client(shared);
        let pending = client.start(&redirect_uri, &host_id, account.as_ref())?;
        Ok(StartedLogin {
            prompt: LoginPrompt::OpenUrl {
                url: pending.url.clone(),
                redirect_uri,
            },
            pending: Arc::new(BrowserLogin {
                client,
                pending,
                store: shared.store.clone(),
            }),
        })
    }

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus> {
        let creds = load(&shared.store).await?;
        Ok(ProviderStatus {
            provider: ID,
            display_name: self.display_name(),
            login_methods: self.login_methods(),
            api_key_set: creds.api_key.is_some(),
            accounts: creds
                .accounts
                .iter()
                .map(|a| AccountStatus {
                    id: a.client_id.clone(),
                    label: a.email.clone(),
                    active: creds.active.as_deref() == Some(a.client_id.as_str()),
                    signed_in: a.tokens.is_some(),
                })
                .collect(),
        })
    }

    async fn logout(&self, shared: &Shared) -> Result<()> {
        let Some(account) = load(&shared.store).await?.active_account().cloned() else {
            return Ok(());
        };
        if let Err(err) = client(shared).revoke(&account).await {
            tracing::warn!(error = %err, "could not revoke the ChatGPT session");
        }
        shared
            .store
            .update_section::<OpenAiCredentials, _, _>(ID.as_str(), move |creds| {
                if let Some(stored) = creds
                    .as_mut()
                    .and_then(|c| c.account_mut(&account.client_id))
                {
                    stored.tokens = None;
                    stored.id_token = None;
                }
            })
            .await
    }

    async fn set_api_key(&self, shared: &Shared, key: Option<String>) -> Result<()> {
        shared
            .store
            .update_section::<OpenAiCredentials, _, _>(ID.as_str(), move |creds| {
                creds.get_or_insert_default().api_key = key
            })
            .await
    }

    async fn select_account(&self, shared: &Shared, account: &str) -> Result<()> {
        let account = account.to_owned();
        shared
            .store
            .update_section::<OpenAiCredentials, _, _>(ID.as_str(), move |creds| {
                let creds = creds.get_or_insert_default();
                if creds.account_mut(&account).is_none() {
                    return Err(LlmError::login(format!("unknown account {account}")));
                }
                creds.active = Some(account);
                Ok(())
            })
            .await?
    }
}
