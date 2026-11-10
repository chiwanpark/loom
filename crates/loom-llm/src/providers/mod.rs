pub mod codex;
pub mod openai;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use loom_config::LlmConfig;

use crate::auth::Auth;
use crate::error::{LlmError, Result};
use crate::provider::{LlmProvider, ProviderId};
pub use codex::CodexProvider;
pub use openai::OpenAiProvider;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub model: String,
}

impl ModelRef {
    pub fn parse(value: &str) -> Result<Self> {
        let (provider, model) = value
            .split_once('/')
            .filter(|(_, model)| !model.is_empty())
            .ok_or_else(|| LlmError::InvalidModel(value.to_owned()))?;
        let provider = ProviderId::parse(provider)
            .ok_or_else(|| LlmError::ProviderDisabled(provider.into()))?;
        Ok(Self {
            provider,
            model: model.to_owned(),
        })
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("loom/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

#[derive(Clone, Default)]
pub struct ProviderSet {
    providers: BTreeMap<&'static str, Arc<dyn LlmProvider>>,
}

impl ProviderSet {
    pub fn from_config(config: &LlmConfig, auth: &Auth, http: &reqwest::Client) -> Self {
        let mut set = Self::default();
        if let Some(openai) = &config.providers.openai {
            set.insert(Arc::new(OpenAiProvider::new(
                http.clone(),
                auth.clone(),
                openai,
            )));
        }
        if let Some(codex) = &config.providers.openai_codex {
            set.insert(Arc::new(CodexProvider::new(
                http.clone(),
                auth.clone(),
                codex,
            )));
        }
        set
    }

    pub fn insert(&mut self, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(provider.id().as_str(), provider);
    }

    pub fn get(&self, id: ProviderId) -> Result<Arc<dyn LlmProvider>> {
        self.providers
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| LlmError::ProviderDisabled(id.as_str().to_owned()))
    }

    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers.values().map(|p| p.id()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_refs() {
        let r = ModelRef::parse("openai-codex/gpt-5.5").unwrap();
        assert_eq!(r.provider, ProviderId::OpenAiCodex);
        assert_eq!(r.model, "gpt-5.5");
        assert_eq!(r.to_string(), "openai-codex/gpt-5.5");
        assert!(ModelRef::parse("gpt-5").is_err());
        assert!(ModelRef::parse("other/gpt-5").is_err());
        assert!(ModelRef::parse("openai/").is_err());
    }

    #[test]
    fn disabled_providers_are_absent() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(
            crate::auth::CredentialStore::in_dir(dir.path()),
            reqwest::Client::new(),
        );
        let mut config = LlmConfig::default();
        config.providers.openai_codex = None;
        let set = ProviderSet::from_config(&config, &auth, &reqwest::Client::new());
        assert_eq!(set.ids(), vec![ProviderId::OpenAi]);
        assert!(set.get(ProviderId::OpenAiCodex).is_err());
    }
}
