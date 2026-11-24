use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::{anthropic, compatible::chat};
use crate::auth::{LentCredential, LoginMethod, LoginMethodInfo, ProviderStatus};
use crate::error::{LlmError, Result};
use crate::kind::{ProviderKind, Shared, StartedLogin, check_http_url, parse_settings};
use crate::provider::{ChunkStream, LlmProvider, ProviderId};
use crate::responses::http::{self, api_error, send_with_retry};
use crate::responses::{Flavor, build_body};
use crate::types::{CompletionRequest, ModelCapabilities, ModelInfo, ReasoningEffort};

const ID: ProviderId = ProviderId::from_static("opencode-go");
const BASE_URL: &str = "https://opencode.ai/inference/go";
const API_KEY_ENV: &str = "OPENCODE_API_KEY";
const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    base_url: Option<String>,
}

impl Settings {
    fn parse(settings: Option<&Value>) -> Result<Self, String> {
        let settings = parse_settings::<Self>(settings)?;
        if let Some(url) = &settings.base_url {
            check_http_url(url).map_err(|err| format!("base_url: {err}"))?;
        }
        Ok(settings)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

async fn api_key(shared: &Shared) -> Result<Option<String>> {
    if let Some(key) = std::env::var(API_KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        return Ok(Some(key));
    }
    Ok(shared
        .store
        .load_section::<Credentials>(ID.as_str())
        .await?
        .and_then(|c| c.api_key))
}

pub(super) struct Kind;

#[async_trait]
impl ProviderKind for Kind {
    fn id(&self) -> ProviderId {
        ID
    }

    fn display_name(&self) -> String {
        "OpenCode Go".into()
    }

    fn login_methods(&self) -> Vec<LoginMethodInfo> {
        vec![LoginMethodInfo::ApiKey]
    }

    fn validate(&self, settings: &Value) -> Result<(), String> {
        Settings::parse(Some(settings)).map(|_| ())
    }

    fn build(&self, settings: Option<&Value>, shared: &Shared) -> Result<Arc<dyn LlmProvider>> {
        let settings = Settings::parse(settings).map_err(|message| LlmError::Config {
            field: format!("llm.providers.{ID}"),
            message,
        })?;
        Ok(Arc::new(Provider {
            shared: shared.clone(),
            base_url: settings
                .base_url
                .as_deref()
                .unwrap_or(BASE_URL)
                .trim_end_matches('/')
                .into(),
            catalog: Mutex::default(),
        }))
    }

    async fn status(&self, shared: &Shared) -> Result<ProviderStatus> {
        Ok(ProviderStatus {
            provider: ID,
            display_name: self.display_name(),
            login_methods: self.login_methods(),
            api_key_set: api_key(shared).await?.is_some(),
            accounts: Vec::new(),
        })
    }

    async fn start_login(&self, _shared: &Shared, _method: LoginMethod) -> Result<StartedLogin> {
        Err(LlmError::Unsupported(
            "OpenCode Go uses an API key instead of a login".into(),
        ))
    }

    async fn logout(&self, shared: &Shared) -> Result<()> {
        self.set_api_key(shared, None).await
    }

    async fn set_api_key(&self, shared: &Shared, key: Option<String>) -> Result<()> {
        shared
            .store
            .update_section::<Credentials, _, _>(ID.as_str(), move |creds| {
                creds.get_or_insert_default().api_key = key;
            })
            .await
    }

    async fn lend(&self, shared: &Shared, _margin_secs: u64) -> Result<Option<LentCredential>> {
        let Some(key) = shared
            .store
            .load_section::<Credentials>(ID.as_str())
            .await?
            .and_then(|c| c.api_key)
        else {
            return Ok(None);
        };
        let lent = Credentials { api_key: Some(key) };
        Ok(Some(LentCredential::new(&lent, None)?))
    }
}

#[derive(Clone, Copy, Default)]
enum Api {
    #[default]
    Chat,
    Responses,
    Messages,
}

impl Api {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/openai/v1/chat/completions",
            Self::Responses => "/openai/v1/responses",
            Self::Messages => "/anthropic/v1/messages",
        }
    }

    fn for_model(model: &Value) -> Self {
        match model["recommended_api"].as_str() {
            Some("messages") => Self::Messages,
            Some("responses" | "interactions" | "generate-content") => Self::Responses,
            Some("chat-completions") => Self::Chat,
            _ if model.pointer("/api/messages").is_some() => Self::Messages,
            _ if ["responses", "interactions", "generate-content"]
                .iter()
                .any(|api| !model["api"][api].is_null()) =>
            {
                Self::Responses
            }
            _ => Self::Chat,
        }
    }
}

struct Model {
    info: ModelInfo,
    api: Api,
    reasoning_supported: Option<bool>,
    reasoning_field: Option<&'static str>,
    xhigh_as_max: bool,
}

impl Model {
    fn parse(model: &Value) -> Option<Self> {
        if model["type"].as_str().is_some_and(|kind| kind != "chat") {
            return None;
        }
        let id = model["id"].as_str().filter(|id| !id.is_empty())?;
        let api = Api::for_model(model);
        let reasoning = &model["capabilities"]["reasoning"];
        let effort = reasoning["effort"].as_array();
        let has_effort = |name| effort.is_some_and(|values| values.iter().any(|v| v == name));
        let xhigh_as_max = has_effort("max") && !has_effort("xhigh");
        let efforts = effort.map(|_| {
            [
                ReasoningEffort::None,
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
            ]
            .into_iter()
            .filter(|e| has_effort(e.as_str()) || (*e == ReasoningEffort::Xhigh && xhigh_as_max))
            .collect()
        });
        let reasoning_supported = reasoning["supported"].as_bool();
        let toggle = reasoning["toggle"]
            .as_bool()
            .or_else(|| effort.map(|_| has_effort("none")));
        let (adaptive_thinking, budget_thinking) = match api {
            Api::Messages => {
                let budget = reasoning["budget"].is_object();
                let adaptive = !budget
                    && ((id.starts_with("claude-") && effort.is_some())
                        || id.starts_with("minimax-m3"));
                (
                    Some(adaptive),
                    Some(budget || (reasoning_supported == Some(true) && !adaptive)),
                )
            }
            _ => (None, None),
        };
        let limit = &model["limit"];
        let positive = |name| limit[name].as_u64().filter(|n| *n > 0);
        let reasoning_field = match model
            .pointer("/api/chat-completions/compatibility/reasoning_field")
            .and_then(Value::as_str)
        {
            Some("reasoning_content" | "reasoning") => Some("reasoning_content"),
            _ if id.starts_with("deepseek-") => Some("reasoning_content"),
            _ => None,
        };
        Some(Self {
            info: ModelInfo {
                id: id.into(),
                display_name: model["name"].as_str().unwrap_or(id).into(),
                capabilities: ModelCapabilities {
                    context_window: positive("input").or_else(|| positive("context")),
                    max_output_tokens: positive("output").and_then(|n| u32::try_from(n).ok()),
                    adaptive_thinking,
                    budget_thinking,
                    thinking_off: toggle,
                    efforts,
                },
            },
            api,
            reasoning_supported,
            reasoning_field,
            xhigh_as_max,
        })
    }

    fn wire_effort(&self, wanted: ReasoningEffort) -> Option<&'static str> {
        if self.reasoning_supported == Some(false) {
            return None;
        }
        let effort = self.info.capabilities.nearest_effort(wanted)?;
        Some(if effort == ReasoningEffort::Xhigh && self.xhigh_as_max {
            "max"
        } else {
            effort.as_str()
        })
    }
}

struct Catalog {
    fetched_at: Instant,
    models: Arc<[Model]>,
}

struct Provider {
    shared: Shared,
    base_url: String,
    catalog: Mutex<Option<Catalog>>,
}

impl Provider {
    async fn catalog(&self) -> Result<Arc<[Model]>> {
        let mut cache = self.catalog.lock().await;
        if let Some(cached) = cache
            .as_ref()
            .filter(|c| c.fetched_at.elapsed() < CATALOG_TTL)
        {
            return Ok(cached.models.clone());
        }
        let response = self
            .shared
            .http
            .get(format!("{}/v1/models", self.base_url))
            .timeout(CATALOG_TIMEOUT)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(ID.as_str(), response).await);
        }
        let body: Value = response.json().await?;
        let mut models: Vec<_> = body["data"]
            .as_array()
            .ok_or_else(|| LlmError::protocol(ID.as_str(), "the model list has no data"))?
            .iter()
            .filter_map(Model::parse)
            .collect();
        models.sort_by(|a, b| a.info.id.cmp(&b.info.id));
        let models: Arc<[Model]> = models.into();
        *cache = Some(Catalog {
            fetched_at: Instant::now(),
            models: models.clone(),
        });
        Ok(models)
    }

    async fn inference_catalog(&self) -> Option<Arc<[Model]>> {
        match self.catalog().await {
            Ok(models) => Some(models),
            Err(error) => {
                tracing::debug!(provider = %ID, %error, "cannot refresh model routes");
                self.catalog.lock().await.as_ref().map(|c| c.models.clone())
            }
        }
    }
}

#[async_trait]
impl LlmProvider for Provider {
    fn id(&self) -> ProviderId {
        ID
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(self
            .catalog()
            .await?
            .iter()
            .map(|model| model.info.clone())
            .collect())
    }

    async fn stream(&self, mut request: CompletionRequest) -> Result<ChunkStream> {
        let key = api_key(&self.shared)
            .await?
            .ok_or_else(|| LlmError::NotAuthenticated {
                provider: ID.to_string(),
                message: format!("set {API_KEY_ENV} or store an API key"),
            })?;
        let catalog = self.inference_catalog().await;
        let model = catalog
            .as_deref()
            .and_then(|models| models.iter().find(|m| m.info.id == request.model));
        let api = model.map(|m| m.api).unwrap_or_default();
        if let Some(model) = model {
            request.capabilities = model.info.capabilities.clone();
        }
        let session = request
            .session_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let effort = request
            .reasoning
            .and_then(|wanted| model.and_then(|m| m.wire_effort(wanted)));
        let body = match api {
            Api::Chat => {
                let provider = &ID;
                let wanted = request.reasoning.take();
                let history = model
                    .and_then(|m| m.reasoning_field)
                    .map(|field| (provider.as_str(), field));
                let mut body = chat::build_body_with_reasoning_history(&request, history);
                if let Some(effort) = effort {
                    body["reasoning_effort"] = json!(effort);
                }
                if request.model.starts_with("deepseek-")
                    && let Some(wanted) = wanted
                {
                    let kind = if wanted == ReasoningEffort::None {
                        "disabled"
                    } else {
                        "enabled"
                    };
                    body["thinking"] = json!({"type": kind});
                    if wanted == ReasoningEffort::None {
                        body.as_object_mut().unwrap().remove("reasoning_effort");
                    }
                }
                body
            }
            Api::Responses => {
                if effort.is_none() {
                    request.reasoning = None;
                }
                let mut body = build_body(ID.as_str(), Flavor::ApiKey, &request);
                if let Some(effort) = effort {
                    body["reasoning"]["effort"] = json!(effort);
                }
                body
            }
            Api::Messages => {
                if model.is_some_and(|m| m.reasoning_supported == Some(false)) {
                    request.reasoning = None;
                }
                let mut body = anthropic::request::build_api_key_body(&request, &ID)?;
                if let Some(effort) = effort
                    && body["output_config"].is_object()
                {
                    body["output_config"]["effort"] = json!(effort);
                }
                body
            }
        };
        let url = format!("{}{}", self.base_url, api.path());
        let response = send_with_retry(ID.as_str(), self.shared.retry, || {
            let builder = self
                .shared
                .http
                .post(&url)
                .bearer_auth(&key)
                .header("accept", "text/event-stream")
                .header("x-opencode-session", &session)
                .json(&body);
            if matches!(api, Api::Messages) {
                builder.header("anthropic-version", "2023-06-01")
            } else {
                builder
            }
        })
        .await?;
        Ok(match api {
            Api::Chat => chat::chunk_stream(ID.as_str(), &request.model, response),
            Api::Responses => http::chunk_stream(ID.as_str(), &request.model, response),
            Api::Messages => {
                let tools = request.tools.into_iter().map(|tool| tool.name).collect();
                anthropic::stream::chunk_stream(ID, &request.model, tools, response)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimax_m3_thinking_is_adaptive_without_effort() {
        let model = Model::parse(&json!({
            "id": "minimax-m3",
            "recommended_api": "messages",
            "capabilities": {"reasoning": {"supported": true, "toggle": true}},
        }))
        .unwrap();
        let request = CompletionRequest {
            model: model.info.id,
            capabilities: model.info.capabilities,
            reasoning: Some(ReasoningEffort::Medium),
            ..Default::default()
        };
        let body = anthropic::request::build_api_key_body(&request, &ID).unwrap();
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert!(body.get("output_config").is_none());
    }
}
