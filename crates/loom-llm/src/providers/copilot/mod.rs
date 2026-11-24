mod auth;
mod inference;

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::{
    AccountInfo, AccountStatus, LentCredential, LoginMethod, LoginMethodInfo, LoginPrompt,
    ProviderStatus,
};
use crate::error::{LlmError, Result};
use crate::kind::{PendingLogin, ProviderKind, Shared, StartedLogin, parse_settings};
use crate::provider::{LlmProvider, ProviderId};

const ID: ProviderId = ProviderId::from_static("github-copilot");

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    base_url: Option<String>,
}

impl Settings {
    fn parse(value: Option<&Value>) -> Result<Self, String> {
        let mut settings = parse_settings::<Self>(value)?;
        if let Some(base) = &mut settings.base_url {
            let url = Url::parse(base).map_err(|_| "base_url must be an HTTP(S) URL")?;
            if !matches!(url.scheme(), "https" | "http")
                || !url.has_host()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(
                    "base_url must be an HTTP(S) URL with no user info, query, or fragment".into(),
                );
            }
            *base = url.as_str().trim_end_matches('/').to_owned();
        }
        Ok(settings)
    }
}

struct DeviceLogin {
    shared: Shared,
    device: auth::Device,
}

#[async_trait]
impl PendingLogin for DeviceLogin {
    async fn wait(&self, cancel: &CancellationToken) -> Result<AccountInfo> {
        let credential = auth::wait(&self.shared, &self.device, cancel).await?;
        let cancel = cancel.clone();
        self.shared
            .store
            .update_section::<auth::Credential, _, _>(ID.as_str(), move |section| {
                if cancel.is_cancelled() {
                    return Err(LlmError::Cancelled);
                }
                *section = Some(credential);
                Ok(())
            })
            .await??;
        Ok(AccountInfo {
            provider: ID,
            account_id: None,
            label: None,
        })
    }
}

pub(super) struct Kind;

#[async_trait]
impl ProviderKind for Kind {
    fn id(&self) -> ProviderId {
        ID
    }

    fn display_name(&self) -> String {
        "GitHub Copilot".into()
    }

    fn login_methods(&self) -> Vec<LoginMethodInfo> {
        vec![LoginMethodInfo::DeviceCode]
    }

    fn validate(&self, settings: &Value) -> Result<(), String> {
        Settings::parse(Some(settings)).map(|_| ())
    }

    fn build(&self, settings: Option<&Value>, shared: &Shared) -> Result<Arc<dyn LlmProvider>> {
        let settings = Settings::parse(settings).map_err(|message| LlmError::Config {
            field: format!("llm.providers.{ID}"),
            message,
        })?;
        Ok(Arc::new(inference::CopilotProvider::new(
            shared.clone(),
            settings.base_url,
        )))
    }

    async fn start_login(&self, shared: &Shared, method: LoginMethod) -> Result<StartedLogin> {
        if method != LoginMethod::DeviceCode {
            return Err(LlmError::Unsupported(
                "GitHub Copilot uses device-code login".into(),
            ));
        }
        let device = auth::start(shared).await?;
        Ok(StartedLogin {
            prompt: LoginPrompt::DeviceCode {
                user_code: device.user_code.clone(),
                verification_uri: device.verification_uri.clone(),
                expires_in_secs: device.expires_in,
            },
            pending: Arc::new(DeviceLogin {
                shared: shared.clone(),
                device,
            }),
        })
    }

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus> {
        let credential = shared
            .store
            .load_section::<auth::Credential>(ID.as_str())
            .await?;
        Ok(ProviderStatus {
            provider: ID,
            display_name: self.display_name(),
            login_methods: self.login_methods(),
            api_key_set: false,
            accounts: credential
                .into_iter()
                .map(|c| AccountStatus {
                    id: "default".into(),
                    label: None,
                    active: true,
                    signed_in: !c.token.is_empty(),
                })
                .collect(),
        })
    }

    async fn logout(&self, shared: &Shared) -> Result<()> {
        shared
            .store
            .update_section::<auth::Credential, _, _>(ID.as_str(), |section| *section = None)
            .await
    }

    async fn lend(&self, shared: &Shared, margin_secs: u64) -> Result<Option<LentCredential>> {
        if shared
            .store
            .load_section::<auth::Credential>(ID.as_str())
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let mut credential = auth::fresh_credential(shared, margin_secs).await?;
        credential.github_token = String::new();
        Ok(Some(LentCredential::new(
            &credential,
            Some(credential.expires_at),
        )?))
    }
}
