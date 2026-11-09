use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{Result, ToolError};
use crate::tool::{
    Risk, Tool, ToolContext, ToolDetails, ToolOutput, ToolSpec, lenient_opt_u64, parse_args,
};
use crate::truncate::{Limits, format_size};
use crate::workspace::{FileAccess, FileStamp};

const BINARY_SNIFF_BYTES: usize = 8192;

#[derive(Debug, Default, Clone)]
pub struct ReadTool {
    limits: Limits,
}

impl ReadTool {
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }
}

#[derive(Debug, Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default, deserialize_with = "lenient_opt_u64")]
    offset: Option<u64>,
    #[serde(default, deserialize_with = "lenient_opt_u64")]
    limit: Option<u64>,
}

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: format!(
                "Read a text file in the workspace. Output is limited to {} lines or {} (whichever is reached first). Use offset and limit to page through large files.",
                self.limits.max_lines,
                format_size(self.limits.max_bytes as u64)
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the workspace root or absolute"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Line number to start reading from (1-indexed)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of lines to read"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: ReadArgs = parse_args(args)?;
        ctx.ensure_not_cancelled()?;
        let path = ctx.workspace.resolve(&args.path)?;
        let display = ctx.workspace.display_path(&path);
        let _guard = ctx
            .workspace
            .locks()
            .lock_file(&path, FileAccess::Read)
            .await;

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::NotFound(PathBuf::from(display)));
            }
            Err(err) => return Err(ToolError::io(format!("cannot read {display}"), err)),
        };
        if metadata.is_dir() {
            return Err(ToolError::failed(format!(
                "{display} is a directory; use the shell tool to list it"
            )));
        }

        let start = args.offset.unwrap_or(1).max(1) as usize;
        let max_lines = args
            .limit
            .map(|limit| (limit.max(1) as usize).min(self.limits.max_lines))
            .unwrap_or(self.limits.max_lines);
        let limits = Limits {
            max_lines,
            max_bytes: self.limits.max_bytes,
        };
        let read_path = path.clone();
        let page = tokio::task::spawn_blocking(move || read_page(&read_path, start, limits))
            .await
            .map_err(|err| ToolError::failed(format!("read task failed: {err}")))?
            .map_err(|err| match err {
                PageError::Binary => {
                    ToolError::failed(format!("{display} looks like a binary file"))
                }
                PageError::Io(err) => ToolError::io(format!("cannot read {display}"), err),
            })?;

        ctx.files.record(&path, FileStamp::from_metadata(&metadata));

        if page.total_lines > 0 && start > page.total_lines {
            return Err(ToolError::InvalidArguments(format!(
                "offset {start} is beyond the end of {display} ({} lines)",
                page.total_lines
            )));
        }

        let mut content = page.text;
        let end_line = start + page.lines.saturating_sub(1);
        let truncated = page.lines == 0 && page.total_lines > 0
            || page.lines > 0 && end_line < page.total_lines;
        if let Some((line, size)) = page.oversized_line {
            content.push_str(&format!(
                "[Line {line} is {}, which exceeds the {} read limit. Use the shell tool to read part of it, for example: sed -n '{line}p' {display} | head -c {}]",
                format_size(size as u64),
                format_size(self.limits.max_bytes as u64),
                self.limits.max_bytes
            ));
        } else if page.total_lines == 0 {
            content.push_str("[File is empty]");
        } else if truncated {
            if !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(&format!(
                "\n[Showing lines {start}-{end_line} of {}. Use offset={} to continue.]",
                page.total_lines,
                end_line + 1
            ));
        }

        Ok(ToolOutput {
            content,
            is_error: false,
            details: ToolDetails::Read {
                path,
                start_line: start,
                end_line: if page.lines == 0 { start } else { end_line },
                total_lines: page.total_lines,
                truncated,
            },
        })
    }
}

#[derive(Debug)]
struct Page {
    text: String,
    lines: usize,
    total_lines: usize,
    oversized_line: Option<(usize, usize)>,
}

#[derive(Debug)]
enum PageError {
    Binary,
    Io(std::io::Error),
}

impl From<std::io::Error> for PageError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

fn read_page(path: &Path, start: usize, limits: Limits) -> Result<Page, PageError> {
    let mut reader = BufReader::new(File::open(path)?);
    {
        let head = reader.fill_buf()?;
        if head[..head.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
            return Err(PageError::Binary);
        }
    }

    let mut collected = Vec::new();
    let mut lines = 0;
    let mut total_lines = 0;
    let mut collecting = true;
    let mut oversized_line = None;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        total_lines += 1;
        if !collecting || total_lines < start {
            continue;
        }
        if lines >= limits.max_lines || collected.len() + buffer.len() > limits.max_bytes {
            if lines == 0 {
                oversized_line = Some((total_lines, buffer.len()));
            }
            collecting = false;
            continue;
        }
        collected.extend_from_slice(&buffer);
        lines += 1;
    }

    let text = String::from_utf8_lossy(&collected).replace("\r\n", "\n");
    Ok(Page {
        text,
        lines,
        total_lines,
        oversized_line,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::workspace::Workspace;

    fn context() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::new(dir.path()).unwrap());
        (dir, ToolContext::new(workspace))
    }

    #[tokio::test]
    async fn reads_whole_file() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let output = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output.content, "one\ntwo\n");
        assert!(matches!(
            output.details,
            ToolDetails::Read {
                start_line: 1,
                end_line: 2,
                total_lines: 2,
                truncated: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn pages_with_offset_and_limit() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
        let output = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert_eq!(
            output.content,
            "2\n3\n\n[Showing lines 2-3 of 5. Use offset=4 to continue.]"
        );
    }

    #[tokio::test]
    async fn truncates_by_bytes() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "aaaa\nbbbb\ncccc\n").unwrap();
        let tool = ReadTool::new(Limits {
            max_lines: 100,
            max_bytes: 10,
        });
        let output = tool.call(&ctx, json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(
            output.content,
            "aaaa\nbbbb\n\n[Showing lines 1-2 of 3. Use offset=3 to continue.]"
        );
    }

    #[tokio::test]
    async fn reports_oversized_line() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "x".repeat(100)).unwrap();
        let tool = ReadTool::new(Limits {
            max_lines: 100,
            max_bytes: 10,
        });
        let output = tool.call(&ctx, json!({"path": "a.txt"})).await.unwrap();
        assert!(output.content.starts_with("[Line 1 is 100B"));
    }

    #[tokio::test]
    async fn normalizes_crlf() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "a\r\nb\r\n").unwrap();
        let output = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output.content, "a\nb\n");
    }

    #[tokio::test]
    async fn reports_empty_file() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let output = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output.content, "[File is empty]");
    }

    #[tokio::test]
    async fn rejects_offset_past_end() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
        let result = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt", "offset": 5}))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn rejects_binary_files() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.bin"), [0u8, 1, 2]).unwrap();
        let result = ReadTool::default()
            .call(&ctx, json!({"path": "a.bin"}))
            .await;
        assert!(matches!(result, Err(ToolError::Failed(_))));
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let (_dir, ctx) = context();
        let result = ReadTool::default()
            .call(&ctx, json!({"path": "missing.txt"}))
            .await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn reads_korean_text() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "안녕하세요\n").unwrap();
        let output = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(output.content, "안녕하세요\n");
    }
}
