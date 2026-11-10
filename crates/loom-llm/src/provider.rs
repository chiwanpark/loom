use std::fmt;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::error::{LlmError, Result};
use crate::types::{AssistantMessage, CompletionChunk, CompletionRequest, ModelInfo};

pub type ChunkStream = BoxStream<'static, Result<CompletionChunk>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderId {
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai-codex")]
    OpenAiCodex,
}

impl ProviderId {
    pub const ALL: [ProviderId; 2] = [ProviderId::OpenAi, ProviderId::OpenAiCodex];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == value)
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    async fn stream(&self, request: CompletionRequest) -> Result<ChunkStream>;

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }
}

pub async fn collect(mut stream: ChunkStream) -> Result<AssistantMessage> {
    while let Some(chunk) = stream.next().await {
        if let CompletionChunk::Done(message) = chunk? {
            return Ok(message);
        }
    }
    Err(LlmError::Stream {
        provider: String::new(),
        code: None,
        message: "stream ended without a final message".into(),
    })
}
