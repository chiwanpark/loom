use std::sync::{Arc, RwLock};

use loom_config::AgentConfig;
use loom_llm::{Auth, HttpClient, LlmProvider, ProviderSet};
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
        http: &HttpClient,
        overrides: &[Arc<dyn LlmProvider>],
    ) -> Self {
        let mut providers = ProviderSet::from_config(&config.llm, auth, http);
        for provider in overrides {
            providers.insert(provider.clone());
        }
        let tools = ToolRegistry::builtin(builtin_tools(&config));
        Self {
            config: Arc::new(config),
            providers,
            tools,
        }
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
