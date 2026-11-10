use async_trait::async_trait;
use serde_json::Value;

use super::{AuthMode, ID, Settings, chatgpt_access_token, stored_api_key};
use crate::error::{LlmError, Result};
use crate::kind::Shared;
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{api_error, chunk_stream, send_with_retry};
use crate::responses::models::listed_models;
use crate::responses::{Flavor, build_body};
use crate::types::{CompletionRequest, ModelInfo};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

pub(crate) struct OpenAiProvider {
    shared: Shared,
    mode: AuthMode,
    api_key_env: String,
    base_url: String,
}

impl OpenAiProvider {
    pub(crate) fn new(shared: Shared, settings: Settings) -> Self {
        Self {
            shared,
            mode: settings.auth,
            api_key_env: settings
                .api_key_env
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned()),
            base_url: settings
                .base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
                .trim_end_matches('/')
                .to_owned(),
        }
    }

    async fn credential(&self) -> Result<(String, Flavor)> {
        match self.mode {
            AuthMode::Chatgpt => Ok((chatgpt_access_token(&self.shared).await?, Flavor::ChatGpt)),
            AuthMode::ApiKey => {
                let env = std::env::var(&self.api_key_env)
                    .ok()
                    .filter(|k| !k.trim().is_empty());
                let key = match env {
                    Some(key) => Some(key),
                    None => stored_api_key(&self.shared).await?,
                };
                key.map(|k| (k, Flavor::ApiKey))
                    .ok_or_else(|| LlmError::NotAuthenticated {
                        provider: ID.to_string(),
                        message: format!("set {} or store an API key", self.api_key_env),
                    })
            }
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> ProviderId {
        ID
    }

    async fn stream(&self, request: CompletionRequest) -> Result<ChunkStream> {
        let id = ID;
        let provider = id.as_str();
        let (token, flavor) = self.credential().await?;
        let body = build_body(provider, flavor, &request);
        let url = format!("{}/responses", self.base_url);
        let response = send_with_retry(provider, self.shared.retry, || {
            self.shared
                .http
                .post(&url)
                .bearer_auth(&token)
                .header("accept", "text/event-stream")
                .json(&body)
        })
        .await?;
        Ok(chunk_stream(provider, &request.model, response))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let id = ID;
        let provider = id.as_str();
        let (token, _) = self.credential().await?;
        let response = self
            .shared
            .http
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(provider, response).await);
        }
        let body: Value = response.json().await?;
        if let Some(models) = body.get("models").and_then(Value::as_array) {
            return Ok(listed_models(models));
        }
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::protocol(provider, "model list has no models or data"))?;
        let mut models: Vec<ModelInfo> = data
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(|id| ModelInfo {
                id: id.to_owned(),
                display_name: id.to_owned(),
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}
