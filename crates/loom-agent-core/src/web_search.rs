//! The `search_web` tool: an OpenAI model searches the web, signed in with
//! the `openai-codex` credentials. Without them the tool is blocked.

use std::time::Duration;

use async_trait::async_trait;
use loom_config::{SearchReasoningEffort, WebSearchToolConfig};
use loom_llm::{
    Auth, Freshness, ReasoningEffort, WebSearch, WebSearchQuery, WebSearchResult, WebSearchSettings,
};
use loom_tools::{Risk, Tool, ToolContext, ToolDetails, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub const SEARCH_WEB_TOOL: &str = "search_web";
const SIGN_IN: &str = "sign in to OpenAI Codex (openai-codex) to search the web";

pub struct SearchWebTool {
    search: WebSearch,
    max_sources: usize,
    max_allowed_sources: usize,
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    #[serde(
        default,
        alias = "max_sources",
        rename = "maxSources",
        deserialize_with = "loom_tools::lenient_opt_u64"
    )]
    max_sources: Option<u64>,
    #[serde(default)]
    freshness: Option<Freshness>,
}

fn reasoning(effort: SearchReasoningEffort) -> ReasoningEffort {
    match effort {
        SearchReasoningEffort::None => ReasoningEffort::None,
        SearchReasoningEffort::Minimal => ReasoningEffort::Minimal,
        SearchReasoningEffort::Low => ReasoningEffort::Low,
        SearchReasoningEffort::Medium => ReasoningEffort::Medium,
        SearchReasoningEffort::High => ReasoningEffort::High,
        SearchReasoningEffort::Xhigh => ReasoningEffort::Xhigh,
    }
}

impl SearchWebTool {
    pub fn new(config: &WebSearchToolConfig, auth: &Auth) -> Self {
        let search = WebSearch::new(
            auth,
            WebSearchSettings {
                model: config.model.trim().to_owned(),
                endpoint: config.endpoint.clone(),
                reasoning: config.reasoning_effort.map(reasoning),
            },
        );
        Self {
            search,
            max_sources: config.max_sources.max(1),
            max_allowed_sources: config.max_allowed_sources.max(1),
            timeout: config.timeout.as_duration(),
        }
    }

    fn query(&self, args: Value) -> Result<WebSearchQuery, ToolError> {
        let args: Args = serde_json::from_value(args)
            .map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
        let query = args.query.trim().to_owned();
        if query.is_empty() {
            return Err(ToolError::InvalidArguments(
                "search_web needs a non-empty query".into(),
            ));
        }
        let requested = args
            .max_sources
            .map_or(self.max_sources, |n| n.max(1) as usize);
        Ok(WebSearchQuery {
            query,
            max_sources: requested.min(self.max_allowed_sources),
            freshness: args.freshness.unwrap_or_default(),
        })
    }
}

#[async_trait]
impl Tool for SearchWebTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SEARCH_WEB_TOOL.into(),
            description: "Search the public web with OpenAI and return a concise summary with sources. Use cached freshness for stable topics and live freshness for time-sensitive queries.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for on the web"
                    },
                    "maxSources": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of sources to return"
                    },
                    "freshness": {
                        "type": "string",
                        "enum": ["cached", "live"],
                        "description": "Use 'cached' for stable topics, 'live' for time-sensitive queries."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn blocked(&self) -> Option<String> {
        (!self.search.signed_in().await).then(|| SIGN_IN.to_owned())
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let query = self.query(args)?;
        let result = tokio::select! {
            result = self.search.search(&query) => {
                result.map_err(|err| ToolError::failed(err.to_string()))?
            }
            _ = tokio::time::sleep(self.timeout) => {
                return Err(ToolError::failed(format!(
                    "the search timed out after {}s",
                    self.timeout.as_secs()
                )));
            }
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        Ok(ToolOutput {
            content: format_result(&result),
            is_error: false,
            details: ToolDetails::WebSearch {
                query: query.query,
                sources: result.sources.len(),
            },
        })
    }
}

/// The summary, then the numbered sources.
fn format_result(result: &WebSearchResult) -> String {
    let mut lines = vec![result.summary.clone()];
    if !result.sources.is_empty() {
        lines.push(String::new());
        lines.push("Sources:".into());
        for (index, source) in result.sources.iter().enumerate() {
            lines.push(format!("{}. {}", index + 1, source.title));
            lines.push(format!("   {}", source.url));
            if !source.snippet.is_empty() {
                lines.push(format!("   {}", source.snippet));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use loom_llm::{CredentialStore, HttpClient, WebSource};

    use super::*;

    fn tool(dir: &std::path::Path) -> SearchWebTool {
        let auth = Auth::new(CredentialStore::in_dir(dir), HttpClient::new());
        SearchWebTool::new(&WebSearchToolConfig::default(), &auth)
    }

    #[tokio::test]
    async fn the_query_takes_its_defaults_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        let query = tool.query(json!({"query": "  tokio  "})).unwrap();
        assert_eq!(
            query,
            WebSearchQuery {
                query: "tokio".into(),
                max_sources: 5,
                freshness: Freshness::Cached,
            }
        );
        let query = tool
            .query(json!({"query": "q", "maxSources": "3", "freshness": "live"}))
            .unwrap();
        assert_eq!((query.max_sources, query.freshness), (3, Freshness::Live));
        assert_eq!(
            tool.query(json!({"query": "q", "max_sources": 50}))
                .unwrap()
                .max_sources,
            10
        );
        assert_eq!(
            tool.query(json!({"query": "q", "maxSources": 0}))
                .unwrap()
                .max_sources,
            1
        );
        for bad in [
            json!({}),
            json!({"query": " "}),
            json!({"query": "q", "freshness": "old"}),
        ] {
            assert!(
                matches!(tool.query(bad.clone()), Err(ToolError::InvalidArguments(_))),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn the_tool_is_blocked_without_the_codex_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let tool = tool(dir.path());
        assert_eq!(tool.blocked().await.as_deref(), Some(SIGN_IN));

        CredentialStore::in_dir(dir.path())
            .update(|c| {
                c.set_section(
                    "openai-codex",
                    Some(json!({
                        "account_id": "a",
                        "access_token": "t",
                        "refresh_token": "r",
                        "expires_at": 0,
                        "saved_at": 0,
                    })),
                )
            })
            .await
            .unwrap();
        assert_eq!(tool.blocked().await, None);
    }

    #[test]
    fn the_result_lists_its_sources_under_the_summary() {
        let result = WebSearchResult {
            summary: "Tokio 1.50 is out.".into(),
            sources: vec![
                WebSource {
                    title: "Tokio blog".into(),
                    url: "https://tokio.rs/blog".into(),
                    snippet: "Release notes".into(),
                },
                WebSource {
                    title: "crates.io".into(),
                    url: "https://crates.io/crates/tokio".into(),
                    snippet: String::new(),
                },
            ],
        };
        assert_eq!(
            format_result(&result),
            "Tokio 1.50 is out.\n\nSources:\n\
             1. Tokio blog\n   https://tokio.rs/blog\n   Release notes\n\
             2. crates.io\n   https://crates.io/crates/tokio"
        );
        let bare = WebSearchResult {
            summary: "s".into(),
            sources: Vec::new(),
        };
        assert_eq!(format_result(&bare), "s");
    }
}
