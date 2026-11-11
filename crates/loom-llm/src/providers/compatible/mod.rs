mod chat;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{LoginMethod, LoginMethodInfo, ProviderStatus};
use crate::error::{LlmError, Result};
use crate::kind::{ProviderKind, Shared, StartedLogin, check_http_url};
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{api_error, chunk_stream, send_with_retry};
use crate::responses::{Flavor, build_body};
use crate::types::{CompletionRequest, ModelCapabilities, ModelInfo};

pub(crate) const TYPE: &str = "openai-compatible";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Api {
    #[default]
    Chat,
    Responses,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(rename = "type")]
    kind: String,
    base_url: String,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    api: Api,
    #[serde(default)]
    models: Vec<ModelEntry>,
    #[serde(default)]
    display_name: Option<String>,
}

/// A configured model: a name, or a name with the limits the server does
/// not report.
#[derive(Debug, Clone)]
pub(crate) enum ModelEntry {
    Name(String),
    Detailed(ModelDetails),
}

// By hand rather than untagged, so a mistyped field is named in the error.
impl<'de> Deserialize<'de> for ModelEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(id) => Ok(Self::Name(id)),
            other => ModelDetails::deserialize(other)
                .map(Self::Detailed)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelDetails {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
}

impl ModelEntry {
    fn id(&self) -> &str {
        match self {
            Self::Name(id) => id,
            Self::Detailed(details) => &details.id,
        }
    }

    fn info(&self) -> ModelInfo {
        match self {
            Self::Name(id) => ModelInfo::named(id.clone()),
            Self::Detailed(details) => ModelInfo {
                id: details.id.clone(),
                display_name: details
                    .display_name
                    .clone()
                    .unwrap_or_else(|| details.id.clone()),
                capabilities: ModelCapabilities {
                    context_window: details.context_window,
                    max_output_tokens: details.max_output_tokens,
                    ..ModelCapabilities::default()
                },
            },
        }
    }
}

impl Settings {
    fn parse(value: &Value) -> Result<Self, String> {
        let settings: Self = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        if settings.kind != TYPE {
            return Err(format!("type must be {TYPE}"));
        }
        check_http_url(&settings.base_url)?;
        if settings.models.iter().any(|m| m.id().trim().is_empty()) {
            return Err("models must not contain empty names".into());
        }
        Ok(settings)
    }
}

pub(crate) fn is_custom(settings: &Value) -> bool {
    settings.get("type").and_then(Value::as_str) == Some(TYPE)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

pub(crate) struct CompatibleKind {
    id: ProviderId,
    settings: Settings,
}

impl CompatibleKind {
    pub(crate) fn new(id: &str, settings: &Value) -> Result<Self, String> {
        Ok(Self {
            id: ProviderId::new(id),
            settings: Settings::parse(settings)?,
        })
    }

    pub(crate) fn validate_settings(settings: &Value) -> Result<(), String> {
        Settings::parse(settings).map(|_| ())
    }
}

async fn stored_key(shared: &Shared, id: &ProviderId) -> Result<Option<String>> {
    Ok(shared
        .store
        .load_section::<Credentials>(id.as_str())
        .await?
        .and_then(|c| c.api_key))
}

fn env_key(settings: &Settings) -> Option<String> {
    settings
        .api_key_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|k| !k.trim().is_empty())
}

#[async_trait]
impl ProviderKind for CompatibleKind {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn display_name(&self) -> String {
        self.settings
            .display_name
            .clone()
            .unwrap_or_else(|| self.id.to_string())
    }

    fn login_methods(&self) -> Vec<LoginMethodInfo> {
        vec![LoginMethodInfo::ApiKey]
    }

    fn validate(&self, settings: &Value) -> Result<(), String> {
        Self::validate_settings(settings)
    }

    fn build(&self, _settings: Option<&Value>, shared: &Shared) -> Result<Arc<dyn LlmProvider>> {
        Ok(Arc::new(CompatibleProvider {
            id: self.id.clone(),
            settings: self.settings.clone(),
            base_url: self.settings.base_url.trim_end_matches('/').to_owned(),
            shared: shared.clone(),
        }))
    }

    async fn start_login(&self, _shared: &Shared, _method: LoginMethod) -> Result<StartedLogin> {
        Err(LlmError::Unsupported(format!(
            "{} uses an API key instead of a login",
            self.id
        )))
    }

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus> {
        let stored = stored_key(shared, &self.id).await?;
        Ok(ProviderStatus {
            provider: self.id.clone(),
            display_name: self.display_name(),
            login_methods: self.login_methods(),
            api_key_set: stored.is_some() || env_key(&self.settings).is_some(),
            accounts: Vec::new(),
        })
    }

    async fn logout(&self, shared: &Shared) -> Result<()> {
        self.set_api_key(shared, None).await
    }

    async fn set_api_key(&self, shared: &Shared, key: Option<String>) -> Result<()> {
        shared
            .store
            .update_section::<Credentials, _, _>(self.id.as_str(), move |creds| {
                creds.get_or_insert_default().api_key = key
            })
            .await
    }
}

struct CompatibleProvider {
    id: ProviderId,
    settings: Settings,
    base_url: String,
    shared: Shared,
}

impl CompatibleProvider {
    async fn key(&self) -> Result<Option<String>> {
        if let Some(key) = env_key(&self.settings) {
            return Ok(Some(key));
        }
        stored_key(&self.shared, &self.id).await
    }

    fn request(
        &self,
        builder: reqwest::RequestBuilder,
        key: Option<&str>,
    ) -> reqwest::RequestBuilder {
        match key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }
}

#[async_trait]
impl LlmProvider for CompatibleProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    async fn stream(&self, mut request: CompletionRequest) -> Result<ChunkStream> {
        let provider = self.id.as_str();
        // A configured output limit is what the server should be told.
        if request.max_output_tokens.is_none() {
            request.max_output_tokens = request.capabilities.max_output_tokens;
        }
        let key = self.key().await?;
        match self.settings.api {
            Api::Responses => {
                let body = build_body(provider, Flavor::ApiKey, &request);
                let url = format!("{}/responses", self.base_url);
                let response = send_with_retry(provider, self.shared.retry, || {
                    self.request(self.shared.http.post(&url), key.as_deref())
                        .header("accept", "text/event-stream")
                        .json(&body)
                })
                .await?;
                Ok(chunk_stream(provider, &request.model, response))
            }
            Api::Chat => {
                let body = chat::build_body(&request);
                let url = format!("{}/chat/completions", self.base_url);
                let response = send_with_retry(provider, self.shared.retry, || {
                    self.request(self.shared.http.post(&url), key.as_deref())
                        .header("accept", "text/event-stream")
                        .json(&body)
                })
                .await?;
                Ok(chat::chunk_stream(provider, &request.model, response))
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if !self.settings.models.is_empty() {
            return Ok(self.settings.models.iter().map(ModelEntry::info).collect());
        }
        let provider = self.id.as_str();
        let key = self.key().await?;
        let response = self
            .request(
                self.shared.http.get(format!("{}/models", self.base_url)),
                key.as_deref(),
            )
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(provider, response).await);
        }
        let body: Value = response.json().await?;
        let mut models: Vec<ModelInfo> = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::protocol(provider, "the model list has no data"))?
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(ModelInfo::named)
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}
