use std::path::{Path, PathBuf};

use loom_client::proto::ToolResultDetails;

use crate::conversation::{Item, ToolStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    File,
    Hunk,
    Added,
    Removed,
    Context,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeEntry {
    Diff(String),
    Opaque { bytes: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub created: bool,
    pub added: usize,
    pub removed: usize,
    pub entries: Vec<ChangeEntry>,
}

pub fn session_changes(items: &[Item]) -> Vec<FileChange> {
    let mut changes: Vec<FileChange> = Vec::new();
    for item in items {
        let Item::Tool(tool) = item else {
            continue;
        };
        if tool.status != ToolStatus::Succeeded {
            continue;
        }
        let Some(details) = tool.result.as_ref().and_then(|r| r.details.as_ref()) else {
            continue;
        };
        let (path, created, entry) = match details {
            ToolResultDetails::Edit { path, diff, .. } => {
                (path, false, ChangeEntry::Diff(diff.clone()))
            }
            ToolResultDetails::WriteWithDiff {
                path,
                created,
                diff,
                ..
            } => (path, *created, ChangeEntry::Diff(diff.clone())),
            ToolResultDetails::Write {
                path,
                created,
                bytes,
            } => (path, *created, ChangeEntry::Opaque { bytes: *bytes }),
            _ => continue,
        };
        let (added, removed) = match &entry {
            ChangeEntry::Diff(diff) => count_lines(diff),
            ChangeEntry::Opaque { .. } => (0, 0),
        };
        match changes.iter_mut().find(|c| &c.path == path) {
            Some(change) => {
                change.added += added;
                change.removed += removed;
                change.entries.push(entry);
            }
            None => changes.push(FileChange {
                path: path.clone(),
                created,
                added,
                removed,
                entries: vec![entry],
            }),
        }
    }
    changes
}

pub fn count_lines(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

fn hunk_start(header: &str, marker: char) -> Option<u32> {
    let field = header
        .split_whitespace()
        .find(|part| part.starts_with(marker))?;
    field[1..].split(',').next()?.parse().ok()
}

pub fn parse_unified(diff: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut old = 0u32;
    let mut new = 0u32;
    let line = |kind, old, new, text: &str| DiffLine {
        kind,
        old,
        new,
        text: text.to_owned(),
    };
    for raw in diff.lines() {
        if raw.starts_with("+++") || raw.starts_with("---") {
            lines.push(line(DiffLineKind::File, None, None, raw));
        } else if raw.starts_with("@@") {
            old = hunk_start(raw, '-').unwrap_or(1);
            new = hunk_start(raw, '+').unwrap_or(1);
            lines.push(line(DiffLineKind::Hunk, None, None, raw));
        } else if let Some(text) = raw.strip_prefix('+') {
            lines.push(line(DiffLineKind::Added, None, Some(new), text));
            new += 1;
        } else if let Some(text) = raw.strip_prefix('-') {
            lines.push(line(DiffLineKind::Removed, Some(old), None, text));
            old += 1;
        } else if raw.starts_with('\\') {
            lines.push(line(DiffLineKind::Note, None, None, raw));
        } else {
            let text = raw.strip_prefix(' ').unwrap_or(raw);
            lines.push(line(DiffLineKind::Context, Some(old), Some(new), text));
            old += 1;
            new += 1;
        }
    }
    lines
}

pub fn change_lines(change: &FileChange) -> Vec<DiffLine> {
    let total = change.entries.len();
    let mut lines = Vec::new();
    for (ix, entry) in change.entries.iter().enumerate() {
        if total > 1 {
            lines.push(DiffLine {
                kind: DiffLineKind::Note,
                old: None,
                new: None,
                text: format!("Change {} of {total}", ix + 1),
            });
        }
        match entry {
            ChangeEntry::Diff(diff) => lines.extend(
                parse_unified(diff)
                    .into_iter()
                    .filter(|l| l.kind != DiffLineKind::File),
            ),
            ChangeEntry::Opaque { bytes } => lines.push(DiffLine {
                kind: DiffLineKind::Note,
                old: None,
                new: None,
                text: format!("Wrote {bytes} bytes; no diff is available for this write."),
            }),
        }
    }
    lines
}

pub fn display_path(path: &Path, workspace: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use loom_client::proto::{Risk, ToolCall, ToolResult};

    use super::*;
    use crate::conversation::ToolItem;

    fn tool(status: ToolStatus, details: ToolResultDetails) -> Item {
        Item::Tool(ToolItem {
            call: ToolCall {
                call_id: "c".into(),
                name: "edit".into(),
                arguments: "{}".into(),
                risk: Some(Risk::Write),
            },
            status,
            approval_reason: None,
            decision: None,
            output: Vec::new(),
            output_truncated: false,
            result: Some(ToolResult {
                content: String::new(),
                is_error: false,
                details: Some(details),
            }),
        })
    }

    const DIFF: &str =
        "--- a.rs\n+++ a.rs\n@@ -2,3 +2,3 @@\n fn a() {\n-    old();\n+    new();\n }\n";

    #[test]
    fn groups_changes_by_path_in_first_touch_order() {
        let items = vec![
            tool(
                ToolStatus::Succeeded,
                ToolResultDetails::WriteWithDiff {
                    path: "/ws/b.rs".into(),
                    bytes: 4,
                    created: true,
                    diff: "--- b.rs\n+++ b.rs\n@@ -0,0 +1 @@\n+new\n".into(),
                },
            ),
            tool(
                ToolStatus::Succeeded,
                ToolResultDetails::Edit {
                    path: "/ws/a.rs".into(),
                    diff: DIFF.into(),
                    first_changed_line: Some(3),
                },
            ),
            tool(
                ToolStatus::Failed,
                ToolResultDetails::Edit {
                    path: "/ws/c.rs".into(),
                    diff: DIFF.into(),
                    first_changed_line: None,
                },
            ),
            tool(
                ToolStatus::Succeeded,
                ToolResultDetails::Write {
                    path: "/ws/a.rs".into(),
                    bytes: 9,
                    created: false,
                },
            ),
        ];
        let changes = session_changes(&items);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, PathBuf::from("/ws/b.rs"));
        assert!(changes[0].created);
        assert_eq!((changes[0].added, changes[0].removed), (1, 0));
        assert_eq!(changes[1].path, PathBuf::from("/ws/a.rs"));
        assert_eq!((changes[1].added, changes[1].removed), (1, 1));
        assert_eq!(changes[1].entries.len(), 2);
        assert_eq!(display_path(&changes[1].path, Path::new("/ws")), "a.rs");
    }

    #[test]
    fn parses_line_numbers_from_hunks() {
        let lines = parse_unified(DIFF);
        let kinds: Vec<_> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::File,
                DiffLineKind::File,
                DiffLineKind::Hunk,
                DiffLineKind::Context,
                DiffLineKind::Removed,
                DiffLineKind::Added,
                DiffLineKind::Context,
            ]
        );
        assert_eq!((lines[3].old, lines[3].new), (Some(2), Some(2)));
        assert_eq!((lines[4].old, lines[4].new), (Some(3), None));
        assert_eq!((lines[5].old, lines[5].new), (None, Some(3)));
        assert_eq!((lines[6].old, lines[6].new), (Some(4), Some(4)));
        assert_eq!(lines[4].text, "    old();");
    }

    #[test]
    fn labels_multiple_changes_and_opaque_writes() {
        let change = FileChange {
            path: "/ws/a.rs".into(),
            created: false,
            added: 1,
            removed: 1,
            entries: vec![
                ChangeEntry::Diff(DIFF.into()),
                ChangeEntry::Opaque { bytes: 9 },
            ],
        };
        let lines = change_lines(&change);
        assert_eq!(lines[0].text, "Change 1 of 2");
        assert_eq!(lines[1].kind, DiffLineKind::Hunk);
        assert!(lines.iter().any(|l| l.text == "Change 2 of 2"));
        assert!(lines.last().unwrap().text.contains("9 bytes"));
    }
}
