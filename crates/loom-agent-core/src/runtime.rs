use std::sync::{Arc, RwLock};

use std::path::Path;

use loom_config::{AgentConfig, SandboxBackend};
use loom_llm::{Auth, LlmError, LlmProvider, ModelRef, ProviderSet};
use loom_tools::sandbox::{LauncherKind, SandboxLauncher, SandboxPolicy, find_bwrap};
use loom_tools::{BuiltinConfig, Limits, ShellConfig, ToolRegistry};

use crate::models::ModelCatalog;
use crate::notes::{NotesEditTool, NotesReadTool, NotesWriteTool};
use crate::tasks::{TasksReadTool, TasksUpdateTool};
use crate::web_search::SearchWebTool;

#[derive(Clone)]
pub struct Runtime {
    pub config: Arc<AgentConfig>,
    pub providers: ProviderSet,
    pub tools: ToolRegistry,
    /// Rebuilt with the runtime, since the providers may have changed.
    pub models: Arc<ModelCatalog>,
}

impl Runtime {
    pub fn build(
        config: AgentConfig,
        auth: &Auth,
        overrides: &[Arc<dyn LlmProvider>],
        sandbox_exe: Option<&Path>,
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
        let mut tools = ToolRegistry::builtin(builtin_tools(&config, sandbox_exe)?);
        tools.register(Arc::new(SearchWebTool::new(&config.tools.web_search, auth)));
        tools.register(Arc::new(NotesReadTool));
        tools.register(Arc::new(NotesEditTool));
        tools.register(Arc::new(NotesWriteTool));
        tools.register(Arc::new(TasksReadTool));
        tools.register(Arc::new(TasksUpdateTool));
        providers.activate(auth);
        Ok(Self {
            config: Arc::new(config),
            providers,
            tools,
            models: Arc::default(),
        })
    }

    pub fn parse_model(&self, model: &str) -> Result<ModelRef, LlmError> {
        let model = ModelRef::parse(model)?;
        self.providers.get(&model.provider)?;
        Ok(model)
    }
}

fn sandbox_launcher(
    config: &AgentConfig,
    sandbox_exe: Option<&Path>,
) -> Result<Option<SandboxLauncher>, LlmError> {
    let sandbox = &config.sandbox;
    let policy = SandboxPolicy {
        writable: sandbox.writable.clone(),
        network: sandbox.network,
    };
    let unavailable = |message: &str| LlmError::Config {
        field: "sandbox.backend".into(),
        message: message.into(),
    };
    let kind = match sandbox.backend {
        SandboxBackend::None => return Ok(None),
        SandboxBackend::Landlock => LauncherKind::Landlock {
            exe: sandbox_exe
                .ok_or_else(|| {
                    unavailable("landlock needs the loom-agent binary to launch commands")
                })?
                .to_path_buf(),
        },
        SandboxBackend::Bubblewrap => LauncherKind::Bubblewrap {
            bwrap: find_bwrap().ok_or_else(|| unavailable("bwrap is not installed"))?,
        },
    };
    Ok(Some(SandboxLauncher { kind, policy }))
}

pub fn builtin_tools(
    config: &AgentConfig,
    sandbox_exe: Option<&Path>,
) -> Result<BuiltinConfig, LlmError> {
    let shell = &config.tools.shell;
    Ok(BuiltinConfig {
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
            pty: shell.pty,
            sandbox: sandbox_launcher(config, sandbox_exe)?,
            max_saved_output: Some(config.sandbox.max_output.as_u64()),
            ..ShellConfig::default()
        },
    })
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
