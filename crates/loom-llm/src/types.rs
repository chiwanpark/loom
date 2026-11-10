use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::ProviderId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContent {
    Text { text: String },
    Image { mime_type: String, data: String },
}

impl InputContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<Value>,
    },
    Reasoning {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_data: Option<Value>,
    },
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    pub total: u64,
}

impl Usage {
    /// Every prompt token of the request. `input` counts only uncached tokens;
    /// cache reads and writes are reported separately by the providers.
    pub fn prompt_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Tokens in the context after the response: the whole prompt plus the output.
    pub fn context_tokens(&self) -> u64 {
        self.prompt_tokens() + self.output
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub provider: ProviderId,
    pub model: String,
    pub content: Vec<AssistantContent>,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

impl AssistantContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            provider_data: None,
        }
    }
}

impl AssistantMessage {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|c| match c {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<InputContent>,
    },
    Assistant(AssistantMessage),
    ToolResult {
        call_id: String,
        tool_name: String,
        content: Vec<InputContent>,
        #[serde(default)]
        is_error: bool,
    },
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![InputContent::text(text)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompletionRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub reasoning: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompletionChunk {
    Start {
        response_id: Option<String>,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        delta: String,
    },
    TextEnd {
        index: usize,
        text: String,
    },
    ReasoningStart {
        index: usize,
    },
    ReasoningDelta {
        index: usize,
        delta: String,
    },
    ReasoningEnd {
        index: usize,
        summary: String,
    },
    ToolCallStart {
        index: usize,
        call_id: String,
        name: String,
    },
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
        call: ToolCall,
    },
    Done(AssistantMessage),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
}
