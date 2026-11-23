use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use super::ID;
use super::auth::{self, Credential, INDIVIDUAL_API};
use crate::error::{LlmError, Result};
use crate::kind::Shared;
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::providers::{anthropic, compatible::chat};
use crate::responses::http::send_with_retry;
use crate::responses::{self, Flavor};
use crate::types::{
    CompletionRequest, InputContent, Message, ModelCapabilities, ModelInfo, ReasoningEffort,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Api {
    Messages,
    Responses,
    Chat,
}

impl Api {
    fn path(self) -> &'static str {
        match self {
            Self::Messages => "/v1/messages",
            Self::Responses => "/responses",
            Self::Chat => "/chat/completions",
        }
    }

    fn for_model(id: &str) -> Self {
        if id.starts_with("claude-") {
            Self::Messages
        } else if ["gpt-", "grok-", "oswe", "mai-"]
            .iter()
            .any(|p| id.starts_with(p))
        {
            Self::Responses
        } else {
            Self::Chat
        }
    }

    fn from_catalog(id: &str, endpoints: &Value) -> Option<Self> {
        let preferred = Self::for_model(id);
        let Some(endpoints) = endpoints.as_array().filter(|items| !items.is_empty()) else {
            return Some(preferred);
        };
        [preferred, Self::Responses, Self::Messages, Self::Chat]
            .into_iter()
            .find(|api| {
                endpoints
                    .iter()
                    .any(|endpoint| endpoint.as_str() == Some(api.path()))
            })
    }
}

pub(super) struct CopilotProvider {
    shared: Shared,
    base_url: Option<String>,
    routes: RwLock<BTreeMap<String, Api>>,
}

impl CopilotProvider {
    pub fn new(shared: Shared, base_url: Option<String>) -> Self {
        Self {
            shared,
            base_url,
            routes: RwLock::new(BTreeMap::new()),
        }
    }

    fn base<'a>(&'a self, credential: &Credential) -> &'a str {
        self.base_url
            .as_deref()
            .unwrap_or_else(|| credential.api_base())
    }
}

fn inference_headers(
    builder: RequestBuilder,
    token: &str,
    request: &CompletionRequest,
) -> RequestBuilder {
    let initiator = match request.messages.last() {
        None | Some(Message::User { .. }) => "user",
        _ => "agent",
    };
    let has_images = request.messages.iter().any(|message| match message {
        Message::User { content } | Message::ToolResult { content, .. } => content
            .iter()
            .any(|block| matches!(block, InputContent::Image { .. })),
        Message::Assistant(_) => false,
    });
    let builder = auth::headers(builder)
        .bearer_auth(token)
        .header("accept", "text/event-stream")
        .header("openai-intent", "conversation-edits")
        .header("x-initiator", initiator);
    if has_images {
        builder.header("copilot-vision-request", "true")
    } else {
        builder
    }
}

fn openai_effort(request: &CompletionRequest, api: Api) -> Option<ReasoningEffort> {
    let wanted = request
        .reasoning
        .filter(|effort| *effort != ReasoningEffort::None)?;
    if let Some(effort) = request.capabilities.nearest_effort(wanted) {
        return (effort != ReasoningEffort::None).then_some(effort);
    }
    (request.capabilities.efforts.is_none()
        && api == Api::Responses
        && ["gpt-5", "gpt-6"]
            .iter()
            .any(|prefix| request.model.starts_with(prefix)))
    .then_some(wanted.clamp(ReasoningEffort::Low, ReasoningEffort::High))
}

#[async_trait]
impl LlmProvider for CopilotProvider {
    fn id(&self) -> ProviderId {
        ID
    }

    async fn stream(&self, mut request: CompletionRequest) -> Result<ChunkStream> {
        let credential = auth::credential(&self.shared).await?;
        let api = self
            .routes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&request.model)
            .copied()
            .unwrap_or_else(|| Api::for_model(&request.model));
        let body = match api {
            Api::Messages => {
                if request.reasoning == Some(ReasoningEffort::None)
                    || (request.capabilities.budget_thinking == Some(false)
                        && request.capabilities.adaptive_thinking != Some(true))
                {
                    request.reasoning = None;
                }
                anthropic::request::build_api_key_body(&request, &ID)?
            }
            Api::Responses => {
                request.reasoning = openai_effort(&request, api);
                responses::build_body(ID.as_str(), Flavor::ApiKey, &request)
            }
            Api::Chat => {
                let effort = openai_effort(&request, api);
                request.reasoning = None;
                let mut body = chat::build_body(&request);
                if let Some(effort) = effort {
                    body["reasoning_effort"] = json!(effort.as_str());
                }
                body
            }
        };
        let url = format!("{}{}", self.base(&credential), api.path());
        let response = send_with_retry(ID.as_str(), self.shared.retry, || {
            let builder =
                inference_headers(self.shared.http.post(&url), &credential.token, &request);
            let builder = if api == Api::Messages {
                builder.header("anthropic-version", "2023-06-01")
            } else {
                builder
            };
            builder.json(&body)
        })
        .await?;
        Ok(match api {
            Api::Messages => {
                anthropic::stream::chunk_stream(ID, &request.model, Vec::new(), response)
            }
            Api::Responses => responses::http::chunk_stream(ID.as_str(), &request.model, response),
            Api::Chat => chat::chunk_stream(ID.as_str(), &request.model, response),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let credential = auth::credential(&self.shared).await?;
        let url = format!("{}/models", self.base(&credential));
        let response = send_with_retry(ID.as_str(), self.shared.retry, || {
            auth::headers(self.shared.http.get(&url))
                .bearer_auth(&credential.token)
                .header("accept", "application/json")
                .header("x-github-api-version", "2026-06-01")
                .timeout(Duration::from_secs(10))
        })
        .await?;
        let body: Value = response.json().await?;
        let catalog = catalog(&body, credential.api_base() == INDIVIDUAL_API)?;
        *self.routes.write().unwrap_or_else(|e| e.into_inner()) = catalog
            .iter()
            .map(|(model, api)| (model.id.clone(), *api))
            .collect();
        Ok(catalog.into_iter().map(|(model, _)| model).collect())
    }
}

fn catalog(body: &Value, individual: bool) -> Result<Vec<(ModelInfo, Api)>> {
    let data = body["data"]
        .as_array()
        .ok_or_else(|| LlmError::protocol(ID.as_str(), "the model list has no data"))?;
    let mut picker = BTreeMap::new();
    let mut fallback = BTreeMap::new();
    for model in data {
        let Some(id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        let caps = &model["capabilities"];
        let supports = &caps["supports"];
        let policy = model["policy"]["state"].as_str();
        if matches!(policy, Some("disabled" | "unconfigured"))
            || caps["type"].as_str().is_some_and(|kind| kind != "chat")
            || supports["tool_calls"].as_bool() == Some(false)
            || supports["streaming"].as_bool() == Some(false)
        {
            continue;
        }
        let Some(api) = Api::from_catalog(id, &model["supported_endpoints"]) else {
            continue;
        };
        let limits = &caps["limits"];
        let positive = |key: &str| limits[key].as_u64().filter(|n| *n > 0);
        let info = ModelInfo {
            id: id.into(),
            display_name: model["name"].as_str().unwrap_or(id).into(),
            capabilities: ModelCapabilities {
                context_window: positive("max_prompt_tokens")
                    .or_else(|| positive("max_context_window_tokens")),
                max_output_tokens: positive("max_output_tokens")
                    .and_then(|n| u32::try_from(n).ok()),
                adaptive_thinking: supports["adaptive_thinking"].as_bool(),
                budget_thinking: supports["thinking"].as_bool(),
                efforts: supports["reasoning_effort"].as_array().map(|efforts| {
                    efforts
                        .iter()
                        .filter_map(|effort| match effort.as_str()? {
                            "none" => Some(ReasoningEffort::None),
                            "minimal" => Some(ReasoningEffort::Minimal),
                            "low" => Some(ReasoningEffort::Low),
                            "medium" => Some(ReasoningEffort::Medium),
                            "high" => Some(ReasoningEffort::High),
                            "xhigh" => Some(ReasoningEffort::Xhigh),
                            _ => None,
                        })
                        .collect()
                }),
                ..ModelCapabilities::default()
            },
        };
        if model["model_picker_enabled"].as_bool() != Some(false) {
            picker.insert(id, (info, api));
        } else if individual && policy == Some("enabled") {
            fallback.insert(id, (info, api));
        }
    }
    Ok(if picker.is_empty() { fallback } else { picker }
        .into_values()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_individual_accounts_fall_back_to_models_with_enabled_policies() {
        let body = json!({"data": [
            {"id": "enabled", "model_picker_enabled": false, "policy": {"state": "enabled"}},
            {"id": "hidden", "model_picker_enabled": false},
            {"id": "disabled", "model_picker_enabled": false, "policy": {"state": "disabled"}},
        ]});
        let models = catalog(&body, true).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0.id, "enabled");
        assert!(catalog(&body, false).unwrap().is_empty());
    }
}
