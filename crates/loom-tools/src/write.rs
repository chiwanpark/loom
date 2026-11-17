use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;

use crate::comments::{CommentGuard, FileText};
use crate::error::{Result, ToolError};
use crate::fsutil::{current_stamp, write_file};
use crate::tool::{Risk, Tool, ToolContext, ToolDetails, ToolOutput, ToolSpec, parse_args};
use crate::truncate::format_size;
use crate::workspace::FileAccess;

const MAX_DIFF_SOURCE: u64 = 1024 * 1024;
const MAX_DIFF: usize = 256 * 1024;

#[derive(Debug, Default, Clone)]
pub struct WriteTool {
    comments: Arc<CommentGuard>,
}

impl WriteTool {
    pub fn new(comments: Arc<CommentGuard>) -> Self {
        Self { comments }
    }
}

fn previous_text(path: &Path) -> Option<String> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_DIFF_SOURCE => {
            String::from_utf8(std::fs::read(path).ok()?).ok()
        }
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    }
}

fn write_diff(display: &str, old: &str, new: &str) -> Option<String> {
    if new.len() as u64 > MAX_DIFF_SOURCE {
        return None;
    }
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(display, display)
        .to_string();
    (diff.len() <= MAX_DIFF).then_some(diff)
}

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
        let path = ctx.resolve(&args.path)?;
        let display = ctx.workspace.display_path(&path);
        if ctx.is_protected(&path) {
            return Err(ToolError::protected(&display));
        }
        let _guard = ctx.lock_file(&path, FileAccess::Write).await;

        let stamp = current_stamp(&path)
            .map_err(|err| ToolError::io(format!("cannot inspect {display}"), err))?;
        ctx.files.ensure_fresh(&path, stamp)?;

        let write_path = path.clone();
        let content = args.content;
        let bytes = content.len();
        let diff_display = display.clone();
        let comments = self.comments.clone();
        let session_guard = ctx.comment_guard;
        let (stamp, created, diff, warning) = tokio::task::spawn_blocking(move || {
            let previous = previous_text(&write_path);
            let warning = match &previous {
                Some(old) => comments
                    .review(
                        session_guard,
                        &[FileText {
                            display: &diff_display,
                            old,
                            new: &content,
                        }],
                    )
                    .map_err(ToolError::failed)?,
                None => None,
            };
            let (stamp, created) = write_file(&write_path, content.as_bytes())
                .map_err(|err| ToolError::io(format!("cannot write {diff_display}"), err))?;
            let diff = previous.and_then(|old| write_diff(&diff_display, &old, &content));
            Ok::<_, ToolError>((stamp, created, diff, warning))
        })
        .await
        .map_err(|err| ToolError::failed(format!("write task failed: {err}")))??;
        ctx.files.record(&path, stamp);

        let verb = if created { "Created" } else { "Wrote" };
        Ok(ToolOutput {
            content: format!(
                "{verb} {display} ({}).{}",
                format_size(bytes as u64),
                warning.unwrap_or_default()
            ),
            is_error: false,
            details: ToolDetails::Write {
                path,
                bytes,
                created,
                diff,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::comments::CommentGuardMode;
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
        let output = WriteTool::default()
            .call(
                &ctx,
                json!({"path": "src/new.rs", "content": "fn x() {}\n"}),
            )
            .await
            .unwrap();
        assert_eq!(output.content, "Created src/new.rs (10B).");
        let ToolDetails::Write { diff, .. } = &output.details else {
            panic!("expected write details");
        };
        let diff = diff.as_deref().unwrap();
        assert!(diff.contains("+fn x() {}"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
            "fn x() {}\n"
        );
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        let output = WriteTool::default()
            .call(&ctx, json!({"path": "a.txt", "content": "new"}))
            .await
            .unwrap();
        assert!(matches!(
            output.details,
            ToolDetails::Write { created: false, .. }
        ));
        let ToolDetails::Write { diff, .. } = &output.details else {
            panic!("expected write details");
        };
        let diff = diff.as_deref().unwrap();
        assert!(diff.contains("-old") && diff.contains("+new"));
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
        let result = WriteTool::default()
            .call(&ctx, json!({"path": "a.txt", "content": "mine"}))
            .await;
        assert!(matches!(result, Err(ToolError::StaleFile { .. })));
    }

    #[tokio::test]
    async fn rejects_paths_outside_workspace() {
        let (_dir, ctx) = context();
        let result = WriteTool::default()
            .call(&ctx, json!({"path": "../escape.txt", "content": "x"}))
            .await;
        assert!(matches!(result, Err(ToolError::OutsideWorkspace(_))));
    }

    #[tokio::test]
    async fn writes_in_allowed_directories_outside_workspace() {
        let outside = tempfile::tempdir().unwrap();
        let allowed = outside.path().canonicalize().unwrap().join("allowed");
        let (_dir, ctx) = context();
        let ctx = ctx.with_allowed(Arc::new(vec![allowed.clone()]));
        let path = allowed.join("sub/new.txt");
        WriteTool::default()
            .call(&ctx, json!({"path": path, "content": "x"}))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
        let escape = allowed.join("../escape.txt");
        let result = WriteTool::default()
            .call(&ctx, json!({"path": escape, "content": "x"}))
            .await;
        assert!(matches!(result, Err(ToolError::OutsideWorkspace(_))));
    }

    fn guarded(mode: CommentGuardMode) -> WriteTool {
        WriteTool::new(Arc::new(CommentGuard {
            mode,
            ..CommentGuard::default()
        }))
    }

    #[tokio::test]
    async fn comment_guard_rejects_or_warns_about_new_comments() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "/// Docs.\nfn a() {}\n").unwrap();
        let args = json!({"path": "a.rs", "content": "/// Docs.\n/// More.\nfn a() {}\n// New.\nfn b() {}\n"});

        let error = guarded(CommentGuardMode::Block)
            .call(&ctx, args.clone())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("a.rs:4: // New."), "{error}");
        assert!(!error.contains("More"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "/// Docs.\nfn a() {}\n"
        );

        let ctx = ctx.with_comment_guard(Some(false));
        let output = guarded(CommentGuardMode::Block)
            .call(&ctx, args.clone())
            .await
            .unwrap();
        assert!(!output.content.contains("comment guard"));

        let (_dir, ctx) = context();
        let output = guarded(CommentGuardMode::Warn)
            .call(&ctx, json!({"path": "b.py", "content": "x = 1  # why\n"}))
            .await
            .unwrap();
        assert!(
            output.content.contains("comment guard found 1 new comment"),
            "{}",
            output.content
        );
    }
}
