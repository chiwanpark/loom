//! The operation thread: a text board each session keeps for the agent's
//! notes on its work. It stays when the conversation is compacted or
//! cleared, and the user can read and edit it too.

use std::sync::Arc;

use async_trait::async_trait;
use loom_proto::FileVersion;
use loom_tools::{
    Replacement, Risk, ThreadAccess, Tool, ToolContext, ToolDetails, ToolError, ToolOutput,
    ToolSpec, apply_edits,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const THREAD_READ_TOOL: &str = "thread_read";
pub const THREAD_EDIT_TOOL: &str = "thread_edit";
pub const THREAD_WRITE_TOOL: &str = "thread_write";

/// Whether `tool` only touches the session's own operation thread.
pub fn is_thread_tool(tool: &str) -> bool {
    [THREAD_READ_TOOL, THREAD_EDIT_TOOL, THREAD_WRITE_TOOL].contains(&tool)
}

/// The version clients see of a thread with `text`: a SHA-256 hash of it.
pub fn version(text: &str) -> FileVersion {
    FileVersion(Sha256::digest(text.as_bytes()).into())
}

const LABEL: &str = "the operation thread";

fn thread(ctx: &ToolContext) -> Result<&Arc<dyn ThreadAccess>, ToolError> {
    ctx.thread
        .as_ref()
        .ok_or_else(|| ToolError::failed("the operation thread is only available in a session"))
}

fn output(content: String, bytes: usize) -> ToolOutput {
    ToolOutput {
        content,
        is_error: false,
        details: ToolDetails::Thread { bytes },
    }
}

fn args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArguments(err.to_string()))
}

pub struct ThreadReadTool;

#[async_trait]
impl Tool for ThreadReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: THREAD_READ_TOOL.into(),
            description: "Read this session's operation thread: your notes on the work, which stay when the conversation is compacted or cleared. The user may have changed it since you last read it.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        let text = thread(ctx)?.read();
        let bytes = text.len();
        let content = if text.is_empty() {
            "The operation thread is empty.".to_owned()
        } else {
            text
        };
        Ok(output(content, bytes))
    }
}

pub struct ThreadEditTool;

#[derive(Deserialize)]
struct EditArgs {
    #[serde(default)]
    edits: Vec<Replacement>,
    #[serde(default, alias = "oldText")]
    old_text: Option<String>,
    #[serde(default, alias = "newText")]
    new_text: Option<String>,
}

impl EditArgs {
    fn replacements(self) -> Result<Vec<Replacement>, ToolError> {
        let mut edits = self.edits;
        match (self.old_text, self.new_text) {
            (Some(old_text), Some(new_text)) => edits.push(Replacement { old_text, new_text }),
            (None, None) => {}
            _ => {
                return Err(ToolError::InvalidArguments(
                    "old_text and new_text must be given together".into(),
                ));
            }
        }
        if edits.is_empty() {
            return Err(ToolError::InvalidArguments(
                "edits must contain at least one replacement".into(),
            ));
        }
        Ok(edits)
    }
}

#[async_trait]
impl Tool for ThreadEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: THREAD_EDIT_TOOL.into(),
            description: "Edit this session's operation thread with exact text replacement. Each edits[].old_text must match exactly one region of the current thread, including whitespace and newlines, and edits must not overlap.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Replacements matched against the current thread, not applied incrementally",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to replace; must be unique in the thread"
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "Replacement text"
                                }
                            },
                            "required": ["old_text", "new_text"]
                        }
                    }
                },
                "required": ["edits"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let edits = self::args::<EditArgs>(args)?.replacements()?;
        let text = thread(ctx)?.edit(&mut |current| apply_edits(current, &edits, LABEL))?;
        Ok(output(
            format!(
                "Edited the operation thread; it is {} bytes now.",
                text.len()
            ),
            text.len(),
        ))
    }
}

pub struct ThreadWriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    #[serde(alias = "content")]
    text: String,
}

#[async_trait]
impl Tool for ThreadWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: THREAD_WRITE_TOOL.into(),
            description: "Replace this session's whole operation thread. Read it first: a thread with changes you have not read is not replaced.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The thread's new text"
                    }
                },
                "required": ["text"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let text = self::args::<WriteArgs>(args)?.text;
        let bytes = text.len();
        thread(ctx)?.write(text)?;
        Ok(output(
            format!("Wrote the operation thread ({bytes} bytes)."),
            bytes,
        ))
    }
}

/// Tells the model what the thread is for. It is fixed text rather than the
/// thread itself, so the system prompt stays the same while the thread
/// changes and the prompt cache keeps working.
pub(crate) const PROMPT_SECTION: &str = "\
# Operation thread

This session has an operation thread: a text board for your notes on the work, \
such as the plan, progress, decisions, and open questions. It stays when the \
conversation is compacted or cleared, and the user can read and edit it too.
- Read it with thread_read when you start a task, and again after the earlier \
conversation was summarized.
- Keep it current as the work moves on: thread_edit changes a part of it, \
thread_write replaces all of it.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_every_thread_tool() {
        for tool in [THREAD_READ_TOOL, THREAD_EDIT_TOOL, THREAD_WRITE_TOOL] {
            assert!(PROMPT_SECTION.contains(tool), "{tool}");
        }
    }
}
