use async_trait::async_trait;

use super::auth::ORIGINATOR;
use super::{ID, Settings, access_token};
use crate::error::Result;
use crate::kind::Shared;
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{chunk_stream, send_with_retry};
use crate::responses::{Flavor, build_body};
use crate::types::CompletionRequest;

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

pub(crate) struct CodexProvider {
    shared: Shared,
    url: String,
}

fn responses_url(base_url: &str) -> String {
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
    pub(crate) fn new(shared: Shared, settings: Settings) -> Self {
        Self {
            shared,
            url: responses_url(settings.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)),
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
