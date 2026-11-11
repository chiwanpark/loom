//! What providers report about their models (context windows, output
//! limits, thinking modes), fetched once and kept for a while.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use loom_llm::{LlmProvider, ModelCapabilities, ModelInfo, ProviderId};
use tokio::time::Instant;

/// How long a fetched model list is used before it is fetched again.
const FRESH: Duration = Duration::from_secs(60 * 60);
/// How long to wait after a failed fetch before trying again.
const RETRY: Duration = Duration::from_secs(5 * 60);
/// A model list that takes longer is given up on, so a slow endpoint never
/// holds up a turn for long.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct ModelCatalog {
    lists: Mutex<HashMap<ProviderId, Fetched>>,
}

struct Fetched {
    at: Instant,
    /// `None` when the fetch failed.
    models: Option<Arc<Vec<ModelInfo>>>,
}

impl Fetched {
    fn is_fresh(&self, now: Instant) -> bool {
        let ttl = if self.models.is_some() { FRESH } else { RETRY };
        now.duration_since(self.at) < ttl
    }
}

impl ModelCatalog {
    /// What the provider reports about `model`, or nothing when it does not
    /// list the model or its list cannot be fetched.
    pub async fn capabilities(&self, provider: &dyn LlmProvider, model: &str) -> ModelCapabilities {
        let Some(models) = self.models(provider).await else {
            return ModelCapabilities::default();
        };
        models
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.capabilities.clone())
            .unwrap_or_default()
    }

    async fn models(&self, provider: &dyn LlmProvider) -> Option<Arc<Vec<ModelInfo>>> {
        let id = provider.id();
        {
            let lists = self.lists.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(fetched) = lists.get(&id).filter(|f| f.is_fresh(Instant::now())) {
                return fetched.models.clone();
            }
        }
        let models = match tokio::time::timeout(TIMEOUT, provider.list_models()).await {
            Ok(Ok(models)) => Some(Arc::new(models)),
            Ok(Err(err)) => {
                tracing::warn!(provider = %id, "cannot list models: {err}");
                None
            }
            Err(_) => {
                tracing::warn!(provider = %id, "listing models timed out");
                None
            }
        };
        let mut lists = self.lists.lock().unwrap_or_else(|e| e.into_inner());
        lists.insert(
            id,
            Fetched {
                at: Instant::now(),
                models: models.clone(),
            },
        );
        models
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use loom_llm::{ChunkStream, CompletionRequest, LlmError};

    use super::*;

    struct Lister {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl LlmProvider for Lister {
        fn id(&self) -> ProviderId {
            ProviderId::new("lister")
        }

        async fn stream(&self, _: CompletionRequest) -> loom_llm::Result<ChunkStream> {
            unreachable!()
        }

        async fn list_models(&self) -> loom_llm::Result<Vec<ModelInfo>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(LlmError::Unsupported("no list".into()));
            }
            let mut model = ModelInfo::named("big");
            model.capabilities.context_window = Some(1_000_000);
            Ok(vec![model, ModelInfo::named("plain")])
        }
    }

    fn lister(fail: bool) -> Lister {
        Lister {
            calls: AtomicUsize::new(0),
            fail,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fetches_once_and_again_when_stale() {
        let catalog = ModelCatalog::default();
        let provider = lister(false);
        let caps = catalog.capabilities(&provider, "big").await;
        assert_eq!(caps.context_window, Some(1_000_000));
        assert_eq!(
            catalog.capabilities(&provider, "plain").await,
            ModelCapabilities::default()
        );
        assert_eq!(
            catalog.capabilities(&provider, "missing").await,
            ModelCapabilities::default()
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(FRESH).await;
        catalog.capabilities(&provider, "big").await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_fetch_waits_before_retrying() {
        let catalog = ModelCatalog::default();
        let provider = lister(true);
        for _ in 0..3 {
            assert_eq!(
                catalog.capabilities(&provider, "big").await,
                ModelCapabilities::default()
            );
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(RETRY).await;
        catalog.capabilities(&provider, "big").await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }
}
