use async_trait::async_trait;
use loom_config::{OpenAiAuth, OpenAiProviderConfig};
use serde_json::Value;

use crate::auth::Auth;
use crate::error::{LlmError, Result};
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{api_error, chunk_stream, send_with_retry};
use crate::responses::{Flavor, RetryPolicy, build_body};
use crate::types::{CompletionRequest, ModelInfo};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";

pub struct OpenAiProvider {
    http: reqwest::Client,
    auth: Auth,
    mode: OpenAiAuth,
    api_key_env: String,
    base_url: String,
    retry: RetryPolicy,
}

impl OpenAiProvider {
    pub fn new(http: reqwest::Client, auth: Auth, config: &OpenAiProviderConfig) -> Self {
        Self {
            http,
            auth,
            mode: config.auth,
            api_key_env: config
                .api_key_env
                .clone()
                .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_owned()),
            base_url: config
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
                .trim_end_matches('/')
                .to_owned(),
            retry: RetryPolicy::default(),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    async fn credential(&self) -> Result<(String, Flavor)> {
        match self.mode {
            OpenAiAuth::Chatgpt => Ok((self.auth.chatgpt_access_token().await?, Flavor::ChatGpt)),
            OpenAiAuth::ApiKey => {
                let env = std::env::var(&self.api_key_env)
                    .ok()
                    .filter(|k| !k.trim().is_empty());
                let key = match env {
                    Some(key) => Some(key),
                    None => self.auth.stored_api_key().await?,
                };
                key.map(|k| (k, Flavor::ApiKey))
                    .ok_or_else(|| LlmError::NotAuthenticated {
                        provider: ProviderId::OpenAi.as_str().to_owned(),
                        message: format!("set {} or store an API key", self.api_key_env),
                    })
            }
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenAi
    }

    async fn stream(&self, request: CompletionRequest) -> Result<ChunkStream> {
        let provider = self.id().as_str();
        let (token, flavor) = self.credential().await?;
        let body = build_body(provider, flavor, &request);
        let url = format!("{}/responses", self.base_url);
        let response = send_with_retry(provider, self.retry, || {
            self.http
                .post(&url)
                .bearer_auth(&token)
                .header("accept", "text/event-stream")
                .json(&body)
        })
        .await?;
        Ok(chunk_stream(provider, &request.model, response))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let provider = self.id().as_str();
        let (token, _) = self.credential().await?;
        let response = self
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
            return Ok(models
                .iter()
                .filter(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
                .filter_map(|m| {
                    let slug = m.get("slug").and_then(Value::as_str)?;
                    let name = m
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(slug);
                    Some(ModelInfo {
                        id: slug.to_owned(),
                        display_name: name.to_owned(),
                    })
                })
                .collect());
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
