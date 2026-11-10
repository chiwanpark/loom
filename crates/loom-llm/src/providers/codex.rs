use async_trait::async_trait;
use loom_config::CodexProviderConfig;

use crate::auth::Auth;
use crate::auth::codex::ORIGINATOR;
use crate::error::Result;
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{chunk_stream, send_with_retry};
use crate::responses::{Flavor, RetryPolicy, build_body};
use crate::types::CompletionRequest;

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

pub struct CodexProvider {
    http: reqwest::Client,
    auth: Auth,
    url: String,
    retry: RetryPolicy,
}

pub fn responses_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        DEFAULT_BASE_URL
    } else {
        base
    };
    if base.ends_with("/codex/responses") {
        base.to_owned()
    } else if base.ends_with("/codex") {
        format!("{base}/responses")
    } else {
        format!("{base}/codex/responses")
    }
}

impl CodexProvider {
    pub fn new(http: reqwest::Client, auth: Auth, config: &CodexProviderConfig) -> Self {
        Self {
            http,
            auth,
            url: responses_url(config.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)),
            retry: RetryPolicy::default(),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

#[async_trait]
impl LlmProvider for CodexProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenAiCodex
    }

    async fn stream(&self, request: CompletionRequest) -> Result<ChunkStream> {
        let provider = self.id().as_str();
        let (token, account_id) = self.auth.codex_access_token().await?;
        let body = build_body(provider, Flavor::Codex, &request);
        let response = send_with_retry(provider, self.retry, || {
            let mut builder = self
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
    }
}
