//! The notes: a text scratchpad each session keeps for the agent's work.
//! They stay when the conversation is compacted or cleared, and the user
//! can read and edit them too.

use std::sync::Arc;

use async_trait::async_trait;
use loom_proto::FileVersion;
use loom_tools::patch::{apply_hunks, parse_hunks};
use loom_tools::{
    NotesAccess, Risk, Tool, ToolContext, ToolDetails, ToolError, ToolOutput, ToolSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const NOTES_READ_TOOL: &str = "notes_read";
pub const NOTES_EDIT_TOOL: &str = "notes_edit";
pub const NOTES_WRITE_TOOL: &str = "notes_write";

/// Whether `tool` only touches the session's own notes.
pub fn is_notes_tool(tool: &str) -> bool {
    [NOTES_READ_TOOL, NOTES_EDIT_TOOL, NOTES_WRITE_TOOL].contains(&tool)
}

/// The version clients see of notes with `text`: a SHA-256 hash of it.
pub fn version(text: &str) -> FileVersion {
    FileVersion(Sha256::digest(text.as_bytes()).into())
}

const LABEL: &str = "the notes";

fn notes(ctx: &ToolContext) -> Result<&Arc<dyn NotesAccess>, ToolError> {
    ctx.notes
        .as_ref()
        .ok_or_else(|| ToolError::failed("the notes are only available in a session"))
}

fn output(content: String, bytes: usize) -> ToolOutput {
    ToolOutput {
        content,
        is_error: false,
        details: ToolDetails::Notes { bytes },
    }
}

fn args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArguments(err.to_string()))
}

pub struct NotesReadTool;

#[async_trait]
impl Tool for NotesReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_READ_TOOL.into(),
            description: "Read this session's notes: your scratchpad for the work, which stays when the conversation is compacted or cleared. The user may have changed it since you last read it.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        let text = notes(ctx)?.read();
        let bytes = text.len();
        let content = if text.is_empty() {
            "The notes are empty.".to_owned()
        } else {
            text
        };
        Ok(output(content, bytes))
    }
}

pub struct NotesEditTool;

#[derive(Deserialize)]
struct EditArgs {
    patch: String,
}

#[async_trait]
impl Tool for NotesEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_EDIT_TOOL.into(),
            description: "Change a part of this session's notes with a patch in unified diff format, without file headers. Each hunk starts with an `@@ -<line>,<count> +<line>,<count> @@` line, and each of its lines starts with ' ' (unchanged), '-' (removed) or '+' (added). Hunks are placed by their unchanged and removed lines, which must match the current notes, so include a few unchanged lines around each change; line numbers only help choose between several matching places.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The hunks to apply to the notes, in unified diff format"
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let hunks = parse_hunks(&self::args::<EditArgs>(args)?.patch)?;
        let text = notes(ctx)?.edit(&mut |current| apply_hunks(current, &hunks, LABEL))?;
        Ok(output(
            format!("Edited the notes; they are {} bytes now.", text.len()),
            text.len(),
        ))
    }
}

pub struct NotesWriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    #[serde(alias = "content")]
    text: String,
}

#[async_trait]
impl Tool for NotesWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_WRITE_TOOL.into(),
            description: "Replace this session's whole notes. Read them first: notes with changes you have not read are not replaced.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The notes' new text"
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
        notes(ctx)?.write(text)?;
        Ok(output(format!("Wrote the notes ({bytes} bytes)."), bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_tools_are_recognised() {
        for tool in [NOTES_READ_TOOL, NOTES_EDIT_TOOL, NOTES_WRITE_TOOL] {
            assert!(is_notes_tool(tool), "{tool}");
        }
        assert!(!is_notes_tool("edit"));
    }
}
