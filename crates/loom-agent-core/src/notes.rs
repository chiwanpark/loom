//! The notes: a text scratchpad each session keeps for the agent's work.
//! They stay when the conversation is compacted or cleared, and the user
//! can read and edit them too.

use std::sync::Arc;

use async_trait::async_trait;
use loom_proto::FileVersion;
use loom_tools::{
    NotesAccess, Replacement, Risk, Tool, ToolContext, ToolDetails, ToolError, ToolOutput,
    ToolSpec, apply_edits,
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
impl Tool for NotesEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_EDIT_TOOL.into(),
            description: "Edit this session's notes with exact text replacement. Each edits[].old_text must match exactly one region of the current notes, including whitespace and newlines, and edits must not overlap.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Replacements matched against the current notes, not applied incrementally",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to replace; must be unique in the notes"
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
        let text = notes(ctx)?.edit(&mut |current| apply_edits(current, &edits, LABEL))?;
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
