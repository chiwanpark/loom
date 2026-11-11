use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{Map, Value};

use super::{CLAUDE_CODE_VERSION, ID, Settings, access_token, request, stream};
use crate::error::{LlmError, Result};
use crate::kind::Shared;
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{api_error, send_with_retry};
use crate::types::{CompletionRequest, ModelCapabilities, ModelInfo, ReasoningEffort};
use crate::usage::{USAGE_TIMEOUT, UsageLimits, parse_anthropic};

pub(super) struct AnthropicProvider {
    shared: Shared,
    base: String,
    usage_url: String,
    settings_url: String,
}

impl AnthropicProvider {
    pub fn new(shared: Shared, settings: Settings) -> Self {
        let base = settings
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/');
        let base = base.strip_suffix("/messages").unwrap_or(base);
        let root = base.strip_suffix("/v1").unwrap_or(base);
        let usage_url = format!("{root}/api/oauth/usage");
        let settings_url = format!("{root}/api/claude_code/settings");
        let base = format!("{root}/v1");
        Self {
            shared,
            base,
            usage_url,
            settings_url,
        }
    }

    fn headers(&self, builder: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
        builder.bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14")
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
            .header("x-app", "cli")
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ID
    }

    async fn stream(&self, request: CompletionRequest) -> Result<ChunkStream> {
        let body = request::build_body(&request)?;
        let token = access_token(&self.shared).await?;
        let response = send_with_retry(ID.as_str(), self.shared.retry, || {
            self.headers(
                self.shared.http.post(format!("{}/messages", self.base)),
                &token,
            )
            .header("accept", "text/event-stream")
            .json(&body)
        })
        .await?;
        let tools = request.tools.iter().map(|t| t.name.clone()).collect();
        Ok(stream::chunk_stream(&request.model, tools, response))
    }

    async fn usage_limits(&self) -> Result<Option<UsageLimits>> {
        let token = access_token(&self.shared).await?;
        let response = self
            .headers(self.shared.http.get(&self.usage_url), &token)
            .header("accept", "application/json")
            .timeout(USAGE_TIMEOUT)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(ID.as_str(), response).await);
        }
        Ok(Some(parse_anthropic(&response.json().await?)))
    }

    /// Claude Code's server-managed settings. The organization's admins
    /// publish them; `204` and `404` mean it publishes none.
    async fn managed_settings(&self) -> Result<Option<Map<String, Value>>> {
        let token = access_token(&self.shared).await?;
        let response = self
            .shared
            .http
            .get(&self.settings_url)
            .bearer_auth(&token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
            .header("accept", "application/json")
            .header("cache-control", "no-cache")
            .timeout(USAGE_TIMEOUT)
            .send()
            .await?;
        let status = response.status();
        if status == StatusCode::NO_CONTENT || status == StatusCode::NOT_FOUND {
            return Ok(Some(Map::new()));
        }
        if !status.is_success() {
            return Err(api_error(ID.as_str(), response).await);
        }
        let mut body: Value = response.json().await?;
        match body.get_mut("settings").map(Value::take) {
            Some(Value::Object(settings)) => Ok(Some(settings)),
            Some(Value::Null) | None => Ok(Some(Map::new())),
            Some(_) => Err(LlmError::protocol(
                ID.as_str(),
                "managed settings are not a JSON object",
            )),
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let token = access_token(&self.shared).await?;
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut url = url::Url::parse(&format!("{}/models", self.base))
                .map_err(|_| LlmError::protocol(ID.as_str(), "invalid model list URL"))?;
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let response = send_with_retry(ID.as_str(), self.shared.retry, || {
                self.headers(self.shared.http.get(url.clone()), &token)
            })
            .await?;
            let body: Value = response.json().await?;
            let data = body["data"]
                .as_array()
                .ok_or_else(|| LlmError::protocol(ID.as_str(), "model list has no data"))?;
            for model in data {
                let id = model["id"]
                    .as_str()
                    .ok_or_else(|| LlmError::protocol(ID.as_str(), "model has no id"))?;
                if models.iter().any(|m: &ModelInfo| m.id == id) {
                    return Err(LlmError::protocol(
                        ID.as_str(),
                        "duplicate model in model list",
                    ));
                }
                models.push(ModelInfo {
                    id: id.into(),
                    display_name: model["display_name"].as_str().unwrap_or(id).into(),
                    capabilities: capabilities(model),
                });
            }
            if body["has_more"].as_bool() != Some(true) {
                break;
            }
            let next = body["last_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    LlmError::protocol(ID.as_str(), "model list has no pagination cursor")
                })?;
            if after.as_deref() == Some(next) || data.is_empty() {
                return Err(LlmError::protocol(
                    ID.as_str(),
                    "model list pagination did not advance",
                ));
            }
            after = Some(next.into());
        }
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }
}
/// The limits and thinking modes the model list reports for a model.
fn capabilities(model: &Value) -> ModelCapabilities {
    let supported = |v: &Value| v["supported"].as_bool();
    let caps = &model["capabilities"];
    let thinking = &caps["thinking"]["types"];
    let effort = &caps["effort"];
    let efforts = (supported(effort) == Some(true)).then(|| {
        [
            ("low", ReasoningEffort::Low),
            ("medium", ReasoningEffort::Medium),
            ("high", ReasoningEffort::High),
            ("xhigh", ReasoningEffort::Xhigh),
        ]
        .into_iter()
        .filter(|(name, _)| supported(&effort[*name]) == Some(true))
        .map(|(_, effort)| effort)
        .collect()
    });
    ModelCapabilities {
        context_window: model["max_input_tokens"].as_u64().filter(|n| *n > 0),
        max_output_tokens: model["max_tokens"]
            .as_u64()
            .filter(|n| *n > 0)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
        adaptive_thinking: supported(&thinking["adaptive"]),
        budget_thinking: supported(&thinking["enabled"]),
        thinking_off: supported(&thinking["disabled"]),
        efforts,
    }
}
