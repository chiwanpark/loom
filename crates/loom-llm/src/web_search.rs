//! Web search through the ChatGPT backend's Codex Responses API, signed in
//! with the `openai-codex` credentials. The searching model is told to answer
//! with a JSON summary and its sources.

use serde_json::{Value, json};

use crate::auth::Auth;
use crate::error::{LlmError, Result};
use crate::kind::Shared;
use crate::providers::codex;
use crate::sse::{SseDecoder, SseEvent};
use crate::types::ReasoningEffort;

pub const DEFAULT_SEARCH_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

/// How fresh the results need to be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Stable topics, where accuracy matters more than recency.
    #[default]
    Cached,
    /// Time-sensitive topics.
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchSettings {
    pub model: String,
    /// The Responses endpoint; `None` is the ChatGPT backend.
    pub endpoint: Option<String>,
    pub reasoning: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchQuery {
    pub query: String,
    pub max_sources: usize,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct WebSource {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchResult {
    pub summary: String,
    pub sources: Vec<WebSource>,
}

#[derive(Clone)]
pub struct WebSearch {
    shared: Shared,
    settings: WebSearchSettings,
}

impl std::fmt::Debug for WebSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearch")
            .field("settings", &self.settings)
            .finish()
    }
}

impl WebSearch {
    pub fn new(auth: &Auth, settings: WebSearchSettings) -> Self {
        Self {
            shared: auth.shared().clone(),
            settings,
        }
    }

    /// Whether the `openai-codex` credentials are stored. A store that
    /// can't be read counts as signed out.
    pub async fn signed_in(&self) -> bool {
        match codex::signed_in(&self.shared).await {
            Ok(signed_in) => signed_in,
            Err(err) => {
                tracing::warn!(error = %err, "could not read the OpenAI Codex credentials");
                false
            }
        }
    }

    pub async fn search(&self, query: &WebSearchQuery) -> Result<WebSearchResult> {
        let id = codex::ID;
        let provider = id.as_str();
        let (token, account_id) = codex::access_token(&self.shared).await?;
        let endpoint = self
            .settings
            .endpoint
            .as_deref()
            .unwrap_or(DEFAULT_SEARCH_ENDPOINT);
        let response = self
            .shared
            .http
            .post(endpoint)
            .bearer_auth(&token)
            .header("chatgpt-account-id", &account_id)
            .header("originator", codex::ORIGINATOR)
            .header("OpenAI-Beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .json(&request_body(&self.settings, query))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 => LlmError::NotAuthenticated {
                    provider: provider.into(),
                    message: "OpenAI rejected the credentials; sign in to OpenAI Codex again"
                        .into(),
                },
                _ => crate::responses::http::api_error(provider, response).await,
            });
        }
        let body = response.bytes().await?;
        let text = output_text(&decode(&body)).map_err(|message| LlmError::Stream {
            provider: provider.into(),
            code: None,
            message,
        })?;
        let mut result =
            parse_result(&text).map_err(|message| LlmError::protocol(provider, message))?;
        result.sources.truncate(query.max_sources);
        Ok(result)
    }
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": { "type": "string" },
            "sources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": { "type": "string" },
                        "url": { "type": "string" },
                        "snippet": { "type": "string" },
                    },
                    "required": ["title", "url", "snippet"],
                },
            },
        },
        "required": ["summary", "sources"],
    })
}

fn instructions(query: &WebSearchQuery) -> String {
    [
        "You are performing web research for a coding agent.",
        "Search the public web and answer the user's query using current online sources.",
        match query.freshness {
            Freshness::Live => "Prioritize the most recent and up-to-date information available.",
            Freshness::Cached => "Cached results are fine; prioritize accuracy over recency.",
        },
        "Return ONLY a JSON object matching this schema:",
        &output_schema().to_string(),
        "Do not wrap the JSON in markdown fences or add any extra commentary.",
        &format!(
            "Keep the summary concise and useful. Limit sources to at most {} items.",
            query.max_sources
        ),
        "Prefer primary or official sources when available.",
        "Each source snippet should be short and directly relevant.",
        "",
        &format!("User query: {}", query.query),
    ]
    .join("\n")
}

fn request_body(settings: &WebSearchSettings, query: &WebSearchQuery) -> Value {
    let mut body = json!({
        "model": settings.model,
        "instructions": instructions(query),
        "input": [{ "role": "user", "content": format!("Search the web for: {}", query.query) }],
        "tools": [{ "type": "web_search" }],
        "store": false,
        "stream": true,
    });
    if let Some(effort) = settings.reasoning {
        body["reasoning"] = json!({ "effort": effort.as_str(), "summary": "auto" });
    }
    body
}

/// The events of a whole server-sent event stream.
fn decode(body: &[u8]) -> Vec<(String, Value)> {
    let mut decoder = SseDecoder::new();
    let mut events = decoder.push(body);
    events.extend(decoder.finish());
    events
        .into_iter()
        .filter_map(|SseEvent { event, data }| {
            let data: Value = serde_json::from_str(&data).ok()?;
            let kind = event
                .filter(|e| !e.is_empty())
                .or_else(|| data["type"].as_str().map(str::to_owned))?;
            Some((kind, data))
        })
        .collect()
}

/// The text the model answered with: its deltas, or else the finished text.
/// A stream without any that reports an error fails with its message.
fn output_text(events: &[(String, Value)]) -> Result<String, String> {
    let deltas: String = events
        .iter()
        .filter(|(kind, _)| kind == "response.output_text.delta")
        .filter_map(|(_, data)| data["delta"].as_str())
        .collect();
    if !deltas.is_empty() {
        return Ok(deltas);
    }
    if let Some(text) = events
        .iter()
        .filter(|(kind, _)| kind == "response.output_text.done")
        .find_map(|(_, data)| data["text"].as_str())
    {
        return Ok(text.to_owned());
    }
    let error = events.iter().find_map(|(kind, data)| match kind.as_str() {
        "response.failed" => data["response"]["error"]["message"].as_str(),
        "error" => data["message"]
            .as_str()
            .or_else(|| data["error"]["message"].as_str()),
        _ => None,
    });
    Err(match error {
        Some(message) => format!("the search failed: {message}"),
        None => "the search returned no text".into(),
    })
}

fn parse_result(text: &str) -> Result<WebSearchResult, String> {
    // Asked not to, the model may still fence its JSON.
    let text = text.trim();
    let text = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|rest| rest.trim_end().strip_suffix("```"))
        .unwrap_or(text);
    let value: Value = serde_json::from_str(text)
        .map_err(|err| format!("the search answer is not JSON: {err}"))?;
    let summary = value["summary"]
        .as_str()
        .ok_or("the search answer has no summary")?
        .trim()
        .to_owned();
    if summary.is_empty() {
        return Err("the search answer has an empty summary".into());
    }
    let sources = value["sources"]
        .as_array()
        .ok_or("the search answer has no sources")?
        .iter()
        .filter_map(|source| serde_json::from_value(source.clone()).ok())
        .collect();
    Ok(WebSearchResult { summary, sources })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(freshness: Freshness) -> WebSearchQuery {
        WebSearchQuery {
            query: "rust 2024 edition".into(),
            max_sources: 3,
            freshness,
        }
    }

    #[test]
    fn the_request_asks_for_a_web_search_and_json() {
        let settings = WebSearchSettings {
            model: "m".into(),
            endpoint: None,
            reasoning: None,
        };
        let body = request_body(&settings, &query(Freshness::Cached));
        assert_eq!(body["model"], "m");
        assert_eq!(body["tools"], json!([{ "type": "web_search" }]));
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["input"][0]["content"],
            "Search the web for: rust 2024 edition"
        );
        assert!(body.get("reasoning").is_none());
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.contains("at most 3 items"));
        assert!(instructions.contains("prioritize accuracy over recency"));
        assert!(instructions.ends_with("User query: rust 2024 edition"));

        let settings = WebSearchSettings {
            reasoning: Some(ReasoningEffort::Low),
            ..settings
        };
        let body = request_body(&settings, &query(Freshness::Live));
        assert_eq!(
            body["reasoning"],
            json!({ "effort": "low", "summary": "auto" })
        );
        assert!(
            body["instructions"]
                .as_str()
                .unwrap()
                .contains("most recent and up-to-date")
        );
    }

    #[test]
    fn the_answer_is_the_deltas_or_else_the_finished_text() {
        let stream = "event: response.output_text.delta\ndata: {\"delta\":\"{\\\"a\"}\n\n\
                      data: {\"type\":\"response.output_text.delta\",\"delta\":\"\\\":1}\"}\n\n\
                      event: response.output_text.done\ndata: {\"text\":\"ignored\"}\n\n\
                      data: [DONE]\n\n";
        assert_eq!(
            output_text(&decode(stream.as_bytes())).unwrap(),
            "{\"a\":1}"
        );

        let stream = "event: response.output_text.done\ndata: {\"text\":\"whole\"}\n\n";
        assert_eq!(output_text(&decode(stream.as_bytes())).unwrap(), "whole");

        let stream = "event: response.failed\n\
                      data: {\"response\":{\"error\":{\"message\":\"model not found\"}}}\n\n";
        assert_eq!(
            output_text(&decode(stream.as_bytes())).unwrap_err(),
            "the search failed: model not found"
        );
        assert_eq!(
            output_text(&decode(b"")).unwrap_err(),
            "the search returned no text"
        );
    }

    #[test]
    fn the_answer_is_parsed_and_bad_sources_are_dropped() {
        let text = r#"```json
        {"summary": " Ships in Rust 1.85. ", "sources": [
            {"title": "Rust Blog", "url": "https://blog.rust-lang.org", "snippet": "s"},
            {"title": "no url"},
            "junk"
        ]}
        ```"#;
        let result = parse_result(text).unwrap();
        assert_eq!(result.summary, "Ships in Rust 1.85.");
        assert_eq!(
            result.sources,
            [WebSource {
                title: "Rust Blog".into(),
                url: "https://blog.rust-lang.org".into(),
                snippet: "s".into(),
            }]
        );
        assert!(parse_result("not json").is_err());
        assert!(parse_result(r#"{"sources": []}"#).is_err());
        assert!(parse_result(r#"{"summary": " ", "sources": []}"#).is_err());
        assert!(parse_result(r#"{"summary": "s"}"#).is_err());
    }
}
