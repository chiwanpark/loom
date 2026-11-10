use std::sync::{Arc, RwLock};

use loom_config::AgentConfig;
use loom_llm::{Auth, LlmError, LlmProvider, ModelRef, ProviderSet};
use loom_tools::{BuiltinConfig, Limits, ShellConfig, ToolRegistry};

#[derive(Clone)]
pub struct Runtime {
    pub config: Arc<AgentConfig>,
    pub providers: ProviderSet,
    pub tools: ToolRegistry,
}

impl Runtime {
    pub fn build(
        config: AgentConfig,
        auth: &Auth,
        overrides: &[Arc<dyn LlmProvider>],
    ) -> Result<Self, LlmError> {
        let mut providers = ProviderSet::from_config(&config.llm, auth)?;
        for provider in overrides {
            providers.insert(provider.clone());
        }
        if let Some(model) = &config.llm.default_model {
            let model = ModelRef::parse(model)?;
            if !providers.contains(&model.provider) {
                return Err(LlmError::Config {
                    field: "llm.default_model".into(),
                    message: format!("provider {} is unknown or disabled", model.provider),
                });
            }
        }
        let tools = ToolRegistry::builtin(builtin_tools(&config));
        Ok(Self {
            config: Arc::new(config),
            providers,
            tools,
        })
    }

    pub fn parse_model(&self, model: &str) -> Result<ModelRef, LlmError> {
        let model = ModelRef::parse(model)?;
        self.providers.get(&model.provider)?;
        Ok(model)
    }
}

pub fn builtin_tools(config: &AgentConfig) -> BuiltinConfig {
    let shell = &config.tools.shell;
    BuiltinConfig {
        read_limits: Limits {
            max_lines: config.tools.read.max_lines,
            max_bytes: config.tools.read.max_bytes.as_usize(),
        },
        shell: ShellConfig {
            shell: shell.program.clone(),
            default_timeout: shell.timeout.map(|t| t.as_duration()),
            limits: Limits {
                max_lines: shell.max_lines,
                max_bytes: shell.max_bytes.as_usize(),
            },
            ..ShellConfig::default()
        },
    }
}

#[derive(Clone)]
pub struct RuntimeCell(Arc<RwLock<Arc<Runtime>>>);

impl RuntimeCell {
    pub fn new(runtime: Runtime) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(runtime))))
    }

    pub fn current(&self) -> Arc<Runtime> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace(&self, runtime: Runtime) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(runtime);
    }
}
