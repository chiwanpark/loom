use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Result, ToolError};
use crate::fsutil::{current_stamp, write_file};
use crate::tool::{Risk, Tool, ToolContext, ToolDetails, ToolOutput, ToolSpec, parse_args};
use crate::truncate::format_size;
use crate::workspace::FileAccess;

#[derive(Debug, Default, Clone)]
pub struct WriteTool;

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "Write a file in the workspace. Creates the file and its parent directories if needed, and replaces the whole file if it exists. Prefer the edit tool for changing part of an existing file.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the workspace root or absolute"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full content of the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: WriteArgs = parse_args(args)?;
        ctx.ensure_not_cancelled()?;
        let path = ctx.workspace.resolve(&args.path)?;
        let display = ctx.workspace.display_path(&path);
        let _guard = ctx
            .workspace
            .locks()
            .lock_file(&path, FileAccess::Write)
            .await;

        let stamp = current_stamp(&path)
            .map_err(|err| ToolError::io(format!("cannot inspect {display}"), err))?;
        ctx.files.ensure_fresh(&path, stamp)?;

        let write_path = path.clone();
        let content = args.content;
        let bytes = content.len();
        let (stamp, created) =
            tokio::task::spawn_blocking(move || write_file(&write_path, content.as_bytes()))
                .await
                .map_err(|err| ToolError::failed(format!("write task failed: {err}")))?
                .map_err(|err| ToolError::io(format!("cannot write {display}"), err))?;
        ctx.files.record(&path, stamp);

        let verb = if created { "Created" } else { "Wrote" };
        Ok(ToolOutput {
            content: format!("{verb} {display} ({}).", format_size(bytes as u64)),
            is_error: false,
            details: ToolDetails::Write {
                path,
                bytes,
                created,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::read::ReadTool;
    use crate::workspace::Workspace;

    fn context() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::new(dir.path()).unwrap());
        (dir, ToolContext::new(workspace))
    }

    #[tokio::test]
    async fn creates_file_with_parents() {
        let (dir, ctx) = context();
        let output = WriteTool
            .call(
                &ctx,
                json!({"path": "src/new.rs", "content": "fn x() {}\n"}),
            )
            .await
            .unwrap();
        assert_eq!(output.content, "Created src/new.rs (10B).");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
            "fn x() {}\n"
        );
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        let output = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "new"}))
            .await
            .unwrap();
        assert!(matches!(
            output.details,
            ToolDetails::Write { created: false, .. }
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn rejects_stale_file() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        std::fs::write(&path, "changed elsewhere").unwrap();
        let result = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "mine"}))
            .await;
        assert!(matches!(result, Err(ToolError::StaleFile { .. })));
    }

    #[tokio::test]
    async fn rejects_paths_outside_workspace() {
        let (_dir, ctx) = context();
        let result = WriteTool
            .call(&ctx, json!({"path": "../escape.txt", "content": "x"}))
            .await;
        assert!(matches!(result, Err(ToolError::OutsideWorkspace(_))));
    }
}
