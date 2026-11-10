use async_trait::async_trait;
use serde_json::Value;

use super::auth::ORIGINATOR;
use super::{ID, Settings, access_token};
use crate::error::{LlmError, Result};
use crate::kind::Shared;
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{api_error, chunk_stream, send_with_retry};
use crate::responses::models::listed_models;
use crate::responses::{Flavor, build_body};
use crate::store::now_unix;
use crate::types::{CompletionRequest, ModelInfo};
use crate::usage::{USAGE_TIMEOUT, UsageLimits, parse_codex};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const CLIENT_VERSION: &str = "1.0.0";

pub(crate) struct CodexProvider {
    shared: Shared,
    url: String,
    models_url: String,
    usage_url: String,
}

fn codex_base(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base
    };
    let base = base.strip_suffix("/responses").unwrap_or(base);
    if base.ends_with("/codex") {
        base.to_owned()
    } else {
        format!("{base}/codex")
    }
}

fn responses_url(base_url: &str) -> String {
    format!("{}/responses", codex_base(base_url))
}

fn models_url(base_url: &str) -> String {
    format!("{}/models", codex_base(base_url))
}

/// The ChatGPT backend reports the plan's limits next to, not under, `/codex`.
fn usage_url(base_url: &str) -> String {
    let base = codex_base(base_url);
    let root = base.strip_suffix("/codex").unwrap_or(&base);
    format!("{root}/wham/usage")
}

impl CodexProvider {
    pub(crate) fn new(shared: Shared, settings: Settings) -> Self {
        let base = settings.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
        Self {
            shared,
            url: responses_url(base),
            models_url: models_url(base),
            usage_url: usage_url(base),
        }
    }
}

#[async_trait]
impl LlmProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ID
    }

    async fn stream(&self, request: CompletionRequest) -> Result<ChunkStream> {
        let id = ID;
        let provider = id.as_str();
        let (token, account_id) = access_token(&self.shared).await?;
        let body = build_body(provider, Flavor::Codex, &request);
        let response = send_with_retry(provider, self.shared.retry, || {
            let mut builder = self
                .shared
                .http
                .post(&self.url)
                .bearer_auth(&token)
                .header("chatgpt-account-id", &account_id)
                .header("originator", ORIGINATOR)
                .header("OpenAI-Beta", "responses=experimental")
                .header("accept", "text/event-stream")
                .json(&body);
            if let Some(session) = &request.session_id {
                builder = builder
                    .header("session-id", session)
                    .header("x-client-request-id", session);
            }
            builder
        })
        .await?;
        Ok(chunk_stream(provider, &request.model, response))
    }

    async fn usage_limits(&self) -> Result<Option<UsageLimits>> {
        let (token, account_id) = access_token(&self.shared).await?;
        let response = self
            .shared
            .http
            .get(&self.usage_url)
            .bearer_auth(&token)
            .header("chatgpt-account-id", &account_id)
            .header("originator", ORIGINATOR)
            .header("accept", "application/json")
            .timeout(USAGE_TIMEOUT)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(ID.as_str(), response).await);
        }
        Ok(Some(parse_codex(&response.json().await?, now_unix())))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let id = ID;
        let provider = id.as_str();
        let (token, account_id) = access_token(&self.shared).await?;
        let response = self
            .shared
            .http
            .get(format!(
                "{}?client_version={CLIENT_VERSION}",
                self.models_url
            ))
            .bearer_auth(&token)
            .header("chatgpt-account-id", &account_id)
            .header("originator", ORIGINATOR)
            .header("accept", "application/json")
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(provider, response).await);
        }
        let body: Value = response.json().await?;
        let models = body
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::protocol(provider, "model list has no models"))?;
        Ok(listed_models(models))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_codex_url() {
        assert_eq!(
            responses_url("https://chatgpt.com/backend-api/"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(responses_url("http://x/codex"), "http://x/codex/responses");
        assert_eq!(
            responses_url("http://x/codex/responses"),
            "http://x/codex/responses"
        );
        assert_eq!(
            models_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/codex/models"
        );
        assert_eq!(
            models_url("http://x/codex/responses"),
            "http://x/codex/models"
        );
        assert_eq!(
            usage_url("https://chatgpt.com/backend-api"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(usage_url("http://x/codex/responses"), "http://x/wham/usage");
    }
}
