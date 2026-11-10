use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use loom_config::{LlmConfig, ProviderSetting};

use crate::HttpClient;
use crate::auth::Auth;
use crate::error::{LlmError, Result};
use crate::kind::{ProviderKind, builtin_kind};
use crate::provider::{LlmProvider, ProviderId};
use crate::providers::compatible::{CompatibleKind, is_custom};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub model: String,
}

impl ModelRef {
    pub fn parse(value: &str) -> Result<Self> {
        let (provider, model) = value
            .split_once('/')
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .ok_or_else(|| LlmError::InvalidModel(value.to_owned()))?;
        Ok(Self {
            provider: ProviderId::new(provider),
            model: model.to_owned(),
        })
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

pub fn http_client() -> HttpClient {
    HttpClient::builder()
        .user_agent(concat!("loom/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

pub fn known_providers() -> Vec<ProviderId> {
    crate::providers::builtin().iter().map(|k| k.id()).collect()
}

fn config_error(id: &str, message: String) -> LlmError {
    LlmError::Config {
        field: format!("llm.providers.{id}"),
        message,
    }
}

pub fn validate_config(config: &LlmConfig) -> Result<()> {
    for id in config.providers.ids() {
        let setting = config.providers.setting(id);
        if let Some(kind) = builtin_kind(id) {
            if let ProviderSetting::Configured(settings) = setting {
                kind.validate(settings)
                    .map_err(|message| config_error(id, message))?;
            }
            continue;
        }
        match setting {
            ProviderSetting::Configured(settings) if is_custom(settings) => {
                CompatibleKind::validate_settings(settings)
                    .map_err(|message| config_error(id, message))?;
            }
            _ => {
                let known: Vec<String> = known_providers().iter().map(|p| p.to_string()).collect();
                return Err(config_error(
                    id,
                    format!(
                        "unknown provider; known providers: {}, or set type: openai-compatible",
                        known.join(", ")
                    ),
                ));
            }
        }
    }
    if let Some(model) = &config.default_model {
        let model = ModelRef::parse(model)?;
        let id = model.provider.as_str();
        let enabled = match config.providers.setting(id) {
            ProviderSetting::Disabled => false,
            ProviderSetting::Default => builtin_kind(id).is_some(),
            ProviderSetting::Configured(_) => true,
        };
        if !enabled {
            return Err(LlmError::Config {
                field: "llm.default_model".into(),
                message: format!("provider {} is unknown or disabled", model.provider),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct ProviderSet {
    providers: BTreeMap<ProviderId, Arc<dyn LlmProvider>>,
    custom: Vec<Arc<dyn ProviderKind>>,
}

impl ProviderSet {
    pub fn from_config(config: &LlmConfig, auth: &Auth) -> Result<Self> {
        validate_config(&LlmConfig {
            default_model: None,
            providers: config.providers.clone(),
        })?;
        let mut set = Self::default();
        for kind in crate::providers::builtin() {
            let id = kind.id();
            let settings = match config.providers.setting(id.as_str()) {
                ProviderSetting::Disabled => continue,
                ProviderSetting::Default => None,
                ProviderSetting::Configured(value) => Some(value),
            };
            set.insert(kind.build(settings, auth.shared())?);
        }
        let mut custom: Vec<Arc<dyn ProviderKind>> = Vec::new();
        for id in config.providers.ids() {
            if builtin_kind(id).is_some() {
                continue;
            }
            if let ProviderSetting::Configured(settings) = config.providers.setting(id) {
                let kind = CompatibleKind::new(id, settings)
                    .map_err(|message| config_error(id, message))?;
                set.insert(kind.build(Some(settings), auth.shared())?);
                custom.push(Arc::new(kind));
            }
        }
        set.custom = custom;
        Ok(set)
    }

    pub fn activate(&self, auth: &Auth) {
        auth.shared().custom.replace(self.custom.clone());
    }

    pub fn insert(&mut self, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(provider.id(), provider);
    }

    pub fn get(&self, id: &ProviderId) -> Result<Arc<dyn LlmProvider>> {
        self.providers
            .get(id)
            .cloned()
            .ok_or_else(|| LlmError::UnknownProvider(id.to_string()))
    }

    pub fn contains(&self, id: &ProviderId) -> bool {
        self.providers.contains_key(id)
    }

    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use loom_config::ProvidersConfig;
    use serde_json::{Value, json};

    use super::*;
    use crate::store::CredentialStore;

    fn llm(providers: &[(&str, Option<Value>)], default_model: Option<&str>) -> LlmConfig {
        LlmConfig {
            default_model: default_model.map(str::to_owned),
            providers: ProvidersConfig(
                providers
                    .iter()
                    .map(|(id, v)| (id.to_string(), v.clone()))
                    .collect(),
            ),
        }
    }

    #[test]
    fn parses_model_refs() {
        let r = ModelRef::parse("some-provider/model-1.5").unwrap();
        assert_eq!(r.provider, "some-provider");
        assert_eq!(r.model, "model-1.5");
        assert_eq!(r.to_string(), "some-provider/model-1.5");
        assert!(ModelRef::parse("model").is_err());
        assert!(ModelRef::parse("provider/").is_err());
        assert!(ModelRef::parse("/model").is_err());
    }

    #[test]
    fn every_known_provider_is_enabled_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(CredentialStore::in_dir(dir.path()), HttpClient::new());
        let set = ProviderSet::from_config(&LlmConfig::default(), &auth).unwrap();
        assert_eq!(set.ids(), known_providers());
    }

    #[test]
    fn null_disables_a_provider() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(CredentialStore::in_dir(dir.path()), HttpClient::new());
        let first = known_providers()[0].clone();
        let config = llm(&[(first.as_str(), None)], None);
        let set = ProviderSet::from_config(&config, &auth).unwrap();
        assert!(!set.contains(&first));
        assert!(set.get(&first).is_err());
        let config = llm(&[(first.as_str(), None)], Some(&format!("{first}/model")));
        let err = validate_config(&config).unwrap_err();
        assert!(err.to_string().contains("llm.default_model"));
    }

    #[test]
    fn rejects_unknown_providers_and_bad_settings() {
        let err = validate_config(&llm(&[("nope", Some(json!({})))], None)).unwrap_err();
        assert!(err.to_string().contains("llm.providers.nope"), "{err}");
        for id in known_providers() {
            let config = llm(&[(id.as_str(), Some(json!({"no_such_field": 1})))], None);
            let err = validate_config(&config).unwrap_err();
            assert!(
                err.to_string().contains(&format!("llm.providers.{id}")),
                "{err}"
            );
        }
        assert!(validate_config(&llm(&[], Some("nope/model"))).is_err());
    }

    #[test]
    fn accepts_the_documented_example_config() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = loom_config::Dirs {
            home: dir.path().to_path_buf(),
            config_home: dir.path().join("config"),
            data_home: dir.path().join("data"),
            state_home: dir.path().join("state"),
            runtime_dir: None,
        };
        let example = include_str!("../../loom-config/tests/fixtures/agent.yaml");
        let config = loom_config::AgentConfig::parse(example, "agent.yaml", &dirs).unwrap();
        validate_config(&config.llm).unwrap();
        let auth = Auth::new(CredentialStore::in_dir(dir.path()), HttpClient::new());
        ProviderSet::from_config(&config.llm, &auth).unwrap();
    }
}
