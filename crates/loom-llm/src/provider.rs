use std::borrow::Cow;
use std::fmt;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::error::{LlmError, Result};
use crate::types::{AssistantMessage, CompletionChunk, CompletionRequest, ModelInfo};

pub type ChunkStream = BoxStream<'static, Result<CompletionChunk>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(Cow<'static, str>);

impl ProviderId {
    pub const fn from_static(id: &'static str) -> Self {
        Self(Cow::Borrowed(id))
    }

    pub fn new(id: impl Into<String>) -> Self {
        Self(Cow::Owned(id.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl PartialEq<str> for ProviderId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ProviderId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
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
