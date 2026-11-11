use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::edit::EditTool;
use crate::error::{Result, ToolError};
use crate::read::ReadTool;
use crate::shell::{ShellConfig, ShellTool};
use crate::tool::{Tool, ToolContext, ToolOutput, ToolSpec};
use crate::truncate::Limits;
use crate::write::WriteTool;

#[derive(Debug, Clone, Default)]
pub struct BuiltinConfig {
    pub read_limits: Limits,
    pub shell: ShellConfig,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtin(config: BuiltinConfig) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ReadTool::new(config.read_limits)));
        registry.register(Arc::new(EditTool));
        registry.register(Arc::new(WriteTool));
        registry.register(Arc::new(ShellTool::new(config.shell)));
        registry
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|tool| tool.spec()).collect()
    }

    /// The tools that are not blocked right now.
    pub async fn available_specs(&self) -> Vec<ToolSpec> {
        let mut specs = Vec::new();
        for tool in self.tools.values() {
            if tool.blocked().await.is_none() {
                specs.push(tool.spec());
            }
        }
        specs
    }

    pub async fn call(&self, ctx: &ToolContext, name: &str, args: Value) -> Result<ToolOutput> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::InvalidArguments(format!("unknown tool: {name}")))?;
        tool.call(ctx, args).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn registers_builtin_tools() {
        let registry = ToolRegistry::builtin(BuiltinConfig::default());
        let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(names, ["edit", "read", "shell", "write"]);
    }

    struct Blockable(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl Tool for Blockable {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "blockable".into(),
                description: String::new(),
                parameters: json!({}),
            }
        }

        fn risk(&self, _: &Value) -> crate::Risk {
            crate::Risk::ReadOnly
        }

        async fn blocked(&self) -> Option<String> {
            self.0
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(|| "blocked".into())
        }

        async fn call(&self, _: &ToolContext, _: Value) -> Result<ToolOutput> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn blocked_tools_are_not_available() {
        let tool = Arc::new(Blockable(true.into()));
        let mut registry = ToolRegistry::builtin(BuiltinConfig::default());
        registry.register(tool.clone());
        let names = |specs: Vec<ToolSpec>| specs.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(registry.specs()),
            ["blockable", "edit", "read", "shell", "write"]
        );
        assert_eq!(
            names(registry.available_specs().await),
            ["edit", "read", "shell", "write"]
        );
        tool.0.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            names(registry.available_specs().await),
            ["blockable", "edit", "read", "shell", "write"]
        );
    }

    #[tokio::test]
    async fn dispatches_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(Arc::new(Workspace::new(dir.path()).unwrap()));
        let registry = ToolRegistry::builtin(BuiltinConfig::default());
        registry
            .call(&ctx, "write", json!({"path": "a.txt", "content": "x"}))
            .await
            .unwrap();
        let output = registry
            .call(&ctx, "read", json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output.content, "x");
        assert!(matches!(
            registry.call(&ctx, "missing", json!({})).await,
            Err(ToolError::InvalidArguments(_))
        ));
    }
}
