mod auth;
mod inference;
mod request;
mod stream;

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
use crate::provider::{LlmProvider, ProviderId};
use auth::{Client, Credential};

pub(crate) const ID: ProviderId = ProviderId::from_static("anthropic");
const CLAUDE_CODE_VERSION: &str = "2.1.322";

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthMode {
    #[default]
    AnthropicOauth,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    #[serde(rename = "auth")]
    _auth: AuthMode,
    base_url: Option<String>,
}

impl Settings {
    fn check(&self) -> Result<(), String> {
        match &self.base_url {
            Some(url) => check_http_url(url).map_err(|e| format!("base_url: {e}")),
            None => Ok(()),
        }
    }
}

fn client(shared: &Shared) -> Client {
    Client::new(
        shared.http.clone(),
        shared.auth_base_url(&ID, Client::default_base()),
    )
}

fn not_signed_in() -> LlmError {
    LlmError::NotAuthenticated {
        provider: ID.to_string(),
        message: "sign in to Anthropic first".into(),
    }
}

async fn access_token(shared: &Shared) -> Result<String> {
    let credential = shared
        .store
        .load_section::<Credential>(ID.as_str())
        .await?
        .ok_or_else(not_signed_in)?;
    if credential.is_fresh() {
        return Ok(credential.access_token);
    }
    let guard = shared.store.lock().await?;
    let credential = guard
        .load_section::<Credential>(ID.as_str())
        .await?
        .ok_or_else(not_signed_in)?;
    if credential.is_fresh() {
        return Ok(credential.access_token);
    }
    let refreshed = client(shared).refresh(&credential).await?;
    guard.save_section(ID.as_str(), Some(&refreshed)).await?;
    Ok(refreshed.access_token)
}

struct BrowserLogin {
    client: Client,
    pending: auth::Pending,
    shared: Shared,
}

#[async_trait]
impl PendingLogin for BrowserLogin {
    async fn complete(&self, input: LoginInput) -> Result<AccountInfo> {
        let text = match input {
            LoginInput::Redirect { url } => url,
            LoginInput::Code { code } => code,
        };
        let credential = self.client.complete(&self.pending, &text).await?;
        let info = AccountInfo {
            provider: ID,
            account_id: credential.account_id.clone(),
            label: credential.label.clone(),
        };
        self.shared
            .store
            .update_section::<Credential, _, _>(ID.as_str(), move |s| *s = Some(credential))
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
        "Anthropic (Claude Pro/Max)".into()
    }

    fn login_methods(&self) -> Vec<LoginMethodInfo> {
        vec![LoginMethodInfo::Browser {
            redirect: RedirectSpec {
                host: "localhost".into(),
                port: Some(auth::CALLBACK_PORT),
                path: "/callback".into(),
            },
        }]
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
        Ok(Arc::new(inference::AnthropicProvider::new(
            shared.clone(),
            settings,
        )))
    }

    async fn start_login(&self, shared: &Shared, method: LoginMethod) -> Result<StartedLogin> {
        let LoginMethod::Browser { redirect_uri, .. } = method else {
            return Err(LlmError::Unsupported(
                "Anthropic does not support device-code login".into(),
            ));
        };
        let client = client(shared);
        let pending = client.start(redirect_uri)?;
        Ok(StartedLogin {
            prompt: LoginPrompt::OpenUrl {
                url: pending.url.clone(),
                redirect_uri: pending.redirect_uri.clone(),
            },
            pending: Arc::new(BrowserLogin {
                client,
                pending,
                shared: shared.clone(),
            }),
        })
    }

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus> {
        let credential = shared.store.load_section::<Credential>(ID.as_str()).await?;
        Ok(ProviderStatus {
            provider: ID,
            display_name: self.display_name(),
            login_methods: self.login_methods(),
            api_key_set: false,
            accounts: credential
                .into_iter()
                .map(|c| AccountStatus {
                    id: c.account_id.unwrap_or_else(|| "default".into()),
                    label: c.label,
                    active: true,
                    signed_in: !c.access_token.is_empty(),
                })
                .collect(),
        })
    }

    async fn logout(&self, shared: &Shared) -> Result<()> {
        shared
            .store
            .update_section::<Credential, _, _>(ID.as_str(), |s| *s = None)
            .await
    }
}
