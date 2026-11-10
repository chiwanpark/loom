use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;

use crate::error::{Result, ToolError};
use crate::fsutil::{current_stamp, write_file};
use crate::tool::{Risk, Tool, ToolContext, ToolDetails, ToolOutput, ToolSpec, parse_args};
use crate::workspace::FileAccess;

const BOM: &str = "\u{feff}";

#[derive(Debug, Default, Clone)]
pub struct EditTool;

#[derive(Debug, Clone, Deserialize)]
pub struct Replacement {
    #[serde(alias = "oldText")]
    pub old_text: String,
    #[serde(alias = "newText")]
    pub new_text: String,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    #[serde(default)]
    edits: Vec<Replacement>,
    #[serde(default, alias = "oldText")]
    old_text: Option<String>,
    #[serde(default, alias = "newText")]
    new_text: Option<String>,
}

impl EditArgs {
    fn replacements(self) -> Result<(String, Vec<Replacement>)> {
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
        Ok((self.path, edits))
    }
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Edit a text file in the workspace with exact text replacement. Each edits[].old_text must match exactly one region of the original file, including whitespace and newlines, and edits must not overlap. Merge nearby changes into one edit. Read the file first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file, relative to the workspace root or absolute"
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Replacements matched against the original file, not applied incrementally",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "Exact text to replace; must be unique in the file"
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
                "required": ["path", "edits"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let (path, edits) = parse_args::<EditArgs>(args)?.replacements()?;
        ctx.ensure_not_cancelled()?;
        let path = ctx.workspace.resolve(&path)?;
        let display = ctx.workspace.display_path(&path);
        let _guard = ctx.lock_file(&path, FileAccess::Write).await;

        let stamp = current_stamp(&path)
            .map_err(|err| ToolError::io(format!("cannot inspect {display}"), err))?;
        if stamp.is_none() {
            return Err(ToolError::NotFound(PathBuf::from(display)));
        }
        ctx.files.ensure_fresh(&path, stamp)?;

        let edit_path = path.clone();
        let edit_display = display.clone();
        let edit_count = edits.len();
        let outcome = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&edit_path)
                .map_err(|err| ToolError::io(format!("cannot read {edit_display}"), err))?;
            let original = String::from_utf8(bytes).map_err(|_| {
                ToolError::failed(format!("{edit_display} is not valid UTF-8 text"))
            })?;
            let (bom, body) = match original.strip_prefix(BOM) {
                Some(body) => (BOM, body),
                None => ("", original.as_str()),
            };
            let crlf = uses_crlf(body);
            let old_content = normalize_to_lf(body);
            let new_content = apply_edits(&old_content, &edits, &edit_display)?;
            let mut output = String::with_capacity(bom.len() + new_content.len());
            output.push_str(bom);
            if crlf {
                output.push_str(&new_content.replace('\n', "\r\n"));
            } else {
                output.push_str(&new_content);
            }
            let (stamp, _) = write_file(&edit_path, output.as_bytes())
                .map_err(|err| ToolError::io(format!("cannot write {edit_display}"), err))?;
            Ok::<_, ToolError>((old_content, new_content, stamp))
        })
        .await
        .map_err(|err| ToolError::failed(format!("edit task failed: {err}")))?;
        let (old_content, new_content, stamp) = outcome?;
        ctx.files.record(&path, stamp);

        let diff = TextDiff::from_lines(&old_content, &new_content)
            .unified_diff()
            .context_radius(3)
            .header(&display, &display)
            .to_string();
        let first_changed_line = first_changed_line(&old_content, &new_content);
        let blocks = if edit_count == 1 { "block" } else { "blocks" };
        Ok(ToolOutput {
            content: format!("Edited {display}: replaced {edit_count} {blocks}."),
            is_error: false,
            details: ToolDetails::Edit {
                path,
                diff,
                first_changed_line,
            },
        })
    }
}

fn uses_crlf(text: &str) -> bool {
    match text.find('\n') {
        Some(index) => index > 0 && text.as_bytes()[index - 1] == b'\r',
        None => false,
    }
}

fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

pub fn apply_edits(content: &str, edits: &[Replacement], display: &str) -> Result<String> {
    let total = edits.len();
    let label = |index: usize| {
        if total == 1 {
            "old_text".to_string()
        } else {
            format!("edits[{index}].old_text")
        }
    };

    let mut matches = Vec::with_capacity(total);
    for (index, edit) in edits.iter().enumerate() {
        let old_text = normalize_to_lf(&edit.old_text);
        if old_text.is_empty() {
            return Err(ToolError::InvalidArguments(format!(
                "{} must not be empty",
                label(index)
            )));
        }
        let mut found = content.match_indices(old_text.as_str());
        let Some((start, _)) = found.next() else {
            return Err(ToolError::failed(format!(
                "could not find {} in {display}; it must match exactly, including whitespace and newlines",
                label(index)
            )));
        };
        let occurrences = 1 + found.count();
        if occurrences > 1 {
            return Err(ToolError::failed(format!(
                "found {occurrences} occurrences of {} in {display}; add more surrounding context to make it unique",
                label(index)
            )));
        }
        matches.push((
            start,
            old_text.len(),
            normalize_to_lf(&edit.new_text),
            index,
        ));
    }

    matches.sort_by_key(|(start, ..)| *start);
    for pair in matches.windows(2) {
        let (start, len, _, first) = &pair[0];
        let (next_start, _, _, second) = &pair[1];
        if start + len > *next_start {
            return Err(ToolError::InvalidArguments(format!(
                "edits[{first}] and edits[{second}] overlap in {display}; merge them into one edit"
            )));
        }
    }

    let mut output = String::with_capacity(content.len());
    let mut cursor = 0;
    for (start, len, new_text, _) in &matches {
        output.push_str(&content[cursor..*start]);
        output.push_str(new_text);
        cursor = start + len;
    }
    output.push_str(&content[cursor..]);

    if output == content {
        return Err(ToolError::failed(format!(
            "no changes made to {display}; the replacement produced identical content"
        )));
    }
    Ok(output)
}

fn first_changed_line(old: &str, new: &str) -> Option<usize> {
    let mut old_lines = old.split_inclusive('\n');
    let mut new_lines = new.split_inclusive('\n');
    let mut line = 1;
    loop {
        match (old_lines.next(), new_lines.next()) {
            (None, None) => return None,
            (a, b) if a != b => return Some(line),
            _ => line += 1,
        }
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

    fn replacement(old_text: &str, new_text: &str) -> Replacement {
        Replacement {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }

    #[test]
    fn applies_multiple_disjoint_edits() {
        let output = apply_edits(
            "a\nb\nc\n",
            &[replacement("c", "C"), replacement("a", "A")],
            "f",
        )
        .unwrap();
        assert_eq!(output, "A\nb\nC\n");
    }

    #[test]
    fn rejects_missing_duplicate_and_overlapping_edits() {
        assert!(apply_edits("abc", &[replacement("x", "y")], "f").is_err());
        assert!(apply_edits("aa", &[replacement("a", "b")], "f").is_err());
        assert!(
            apply_edits(
                "abcdef",
                &[replacement("abc", "x"), replacement("cde", "y")],
                "f"
            )
            .is_err()
        );
        assert!(apply_edits("abc", &[replacement("", "y")], "f").is_err());
        assert!(apply_edits("abc", &[replacement("b", "b")], "f").is_err());
    }

    #[test]
    fn finds_first_changed_line() {
        assert_eq!(first_changed_line("a\nb\nc\n", "a\nB\nc\n"), Some(2));
        assert_eq!(first_changed_line("a\n", "a\nb\n"), Some(2));
        assert_eq!(first_changed_line("a\n", "a\n"), None);
    }

    #[tokio::test]
    async fn edits_file_and_reports_diff() {
        let (dir, ctx) = context();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let output = EditTool
            .call(
                &ctx,
                json!({"path": "main.rs", "edits": [{"old_text": "\"hi\"", "new_text": "\"안녕\""}]}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    println!(\"안녕\");\n}\n"
        );
        let ToolDetails::Edit {
            diff,
            first_changed_line,
            ..
        } = output.details
        else {
            panic!("unexpected details");
        };
        assert_eq!(first_changed_line, Some(2));
        assert!(diff.contains("-    println!(\"hi\");"));
        assert!(diff.contains("+    println!(\"안녕\");"));
    }

    #[tokio::test]
    async fn accepts_single_edit_shorthand() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello world").unwrap();
        EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "oldText": "world", "newText": "loom"}),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello loom");
    }

    #[tokio::test]
    async fn preserves_crlf_and_bom() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "\u{feff}one\r\ntwo\r\n").unwrap();
        EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "edits": [{"old_text": "one\ntwo", "new_text": "1\n2"}]}),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "\u{feff}1\r\n2\r\n"
        );
    }

    #[tokio::test]
    async fn rejects_stale_file() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let result = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "edits": [{"old_text": "one", "new_text": "1"}]}),
            )
            .await;
        assert!(matches!(result, Err(ToolError::StaleFile { .. })));
    }

    #[tokio::test]
    async fn allows_consecutive_edits_by_same_session() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a b c").unwrap();
        for (old_text, new_text) in [("a", "A"), ("b", "B"), ("c", "C")] {
            EditTool
                .call(
                    &ctx,
                    json!({"path": "a.txt", "edits": [{"old_text": old_text, "new_text": new_text}]}),
                )
                .await
                .unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A B C");
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let (_dir, ctx) = context();
        let result = EditTool
            .call(
                &ctx,
                json!({"path": "missing.txt", "edits": [{"old_text": "a", "new_text": "b"}]}),
            )
            .await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }
}
