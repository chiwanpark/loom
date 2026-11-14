use std::path::{Path, PathBuf};

use async_trait::async_trait;
use loom_proto::{FileChangeKind, PatchedFile};
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;

use crate::error::{Result, ToolError};
use crate::fsutil::{current_stamp, write_file};
use crate::patch::{FilePatch, Hunk, apply_hunks, parse_patch};
use crate::tool::{Risk, Tool, ToolContext, ToolDetails, ToolOutput, ToolSpec, parse_args};
use crate::workspace::{FileAccess, FileStamp, Workspace};

const BOM: &str = "\u{feff}";
const MAX_DIFF: usize = 256 * 1024;

#[derive(Debug, Default, Clone)]
pub struct EditTool;

#[derive(Debug, Deserialize)]
struct EditArgs {
    patch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Create,
    Modify,
    Delete,
    Move,
}

/// One file section of the patch, with its paths resolved.
#[derive(Debug)]
struct Planned {
    kind: Kind,
    /// The file the change reads: none for a created file.
    source: Option<PathBuf>,
    /// The file the change writes: none for a deleted file.
    target: Option<PathBuf>,
    hunks: Vec<Hunk>,
}

impl Planned {
    fn paths(&self) -> impl Iterator<Item = &Path> {
        let target = self
            .target
            .iter()
            .filter(|target| self.source.as_ref() != Some(target));
        self.source.iter().chain(target).map(PathBuf::as_path)
    }
}

/// A planned change with its old and new text, ready to be written.
#[derive(Debug)]
struct Prepared {
    planned: Planned,
    old_text: String,
    new_text: String,
    /// The bytes to write, with the original BOM and line endings.
    bytes: Vec<u8>,
}

fn invalid(message: String) -> ToolError {
    ToolError::InvalidArguments(message)
}

/// Resolves a path from a file header. Git-style `a/` and `b/` prefixes are
/// dropped, unless only the path with the prefix exists.
fn resolve_header(workspace: &Workspace, raw: &str) -> Result<PathBuf> {
    let stripped = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .filter(|rest| !rest.is_empty());
    let Some(stripped) = stripped else {
        return workspace.resolve(raw);
    };
    let path = workspace.resolve(stripped)?;
    if !path.exists()
        && let Ok(whole) = workspace.resolve(raw)
        && whole.exists()
    {
        return Ok(whole);
    }
    Ok(path)
}

fn plan(workspace: &Workspace, file: FilePatch) -> Result<Planned> {
    let source = file
        .old_path
        .as_deref()
        .map(|raw| resolve_header(workspace, raw))
        .transpose()?;
    let target = file
        .new_path
        .as_deref()
        .map(|raw| resolve_header(workspace, raw))
        .transpose()?;
    let kind = match (&source, &target) {
        (None, Some(_)) => Kind::Create,
        (Some(_), None) => Kind::Delete,
        (Some(source), Some(target)) if source == target => Kind::Modify,
        _ => Kind::Move,
    };
    let (source, target) = match kind {
        Kind::Modify => (source.clone(), source),
        _ => (source, target),
    };
    Ok(Planned {
        kind,
        source,
        target,
        hunks: file.hunks,
    })
}

fn uses_crlf(text: &str) -> bool {
    match text.find('\n') {
        Some(index) => index > 0 && text.as_bytes()[index - 1] == b'\r',
        None => false,
    }
}

/// Reads a text file as `(bom, crlf, text with \n line endings)`.
fn read_text(path: &Path, display: &str) -> Result<(&'static str, bool, String)> {
    let bytes =
        std::fs::read(path).map_err(|err| ToolError::io(format!("cannot read {display}"), err))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ToolError::failed(format!("{display} is not valid UTF-8 text")))?;
    let (bom, body) = match text.strip_prefix(BOM) {
        Some(body) => (BOM, body),
        None => ("", text.as_str()),
    };
    let crlf = uses_crlf(body);
    Ok((bom, crlf, body.replace("\r\n", "\n")))
}

fn prepare(workspace: &Workspace, planned: Planned) -> Result<Prepared> {
    let display = |path: &Option<PathBuf>| {
        path.as_deref()
            .map(|path| workspace.display_path(path))
            .unwrap_or_default()
    };
    let (bom, crlf, old_text) = match &planned.source {
        Some(source) => read_text(source, &display(&planned.source))?,
        None => ("", false, String::new()),
    };
    let label = display(&planned.target.clone().or(planned.source.clone()));
    let new_text = if planned.hunks.is_empty() {
        old_text.clone()
    } else {
        apply_hunks(&old_text, &planned.hunks, &label)?
    };
    let new_text = match planned.kind {
        Kind::Delete => String::new(),
        _ => new_text,
    };
    let mut bytes = String::with_capacity(bom.len() + new_text.len());
    bytes.push_str(bom);
    if crlf {
        bytes.push_str(&new_text.replace('\n', "\r\n"));
    } else {
        bytes.push_str(&new_text);
    }
    Ok(Prepared {
        planned,
        old_text,
        new_text,
        bytes: bytes.into_bytes(),
    })
}

/// What one written file leaves behind: the stamps to record and the paths
/// to forget in the file tracker.
#[derive(Debug, Default)]
struct Written {
    record: Vec<(PathBuf, FileStamp)>,
    forget: Vec<PathBuf>,
}

fn write_one(prepared: &Prepared, written: &mut Written) -> std::io::Result<()> {
    let planned = &prepared.planned;
    if let Some(target) = &planned.target {
        let (stamp, _) = write_file(target, &prepared.bytes)?;
        written.record.push((target.clone(), stamp));
    }
    if matches!(planned.kind, Kind::Delete | Kind::Move)
        && let Some(source) = &planned.source
    {
        std::fs::remove_file(source)?;
        written.forget.push(source.clone());
    }
    Ok(())
}

fn diff_text(old_name: &str, new_name: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(old_name, new_name)
        .to_string();
    if diff.len() <= MAX_DIFF {
        return diff;
    }
    format!(
        "--- {old_name}\n+++ {new_name}\n\\ The diff is too large to show ({} bytes).\n",
        diff.len()
    )
}

fn patched_file(workspace: &Workspace, prepared: &Prepared) -> PatchedFile {
    let planned = &prepared.planned;
    let name = |path: &Option<PathBuf>| match path {
        Some(path) => workspace.display_path(path),
        None => crate::patch::DEV_NULL.to_owned(),
    };
    let diff = diff_text(
        &name(&planned.source),
        &name(&planned.target),
        &prepared.old_text,
        &prepared.new_text,
    );
    let first_changed_line =
        first_changed_line(&prepared.old_text, &prepared.new_text).map(|line| line as u64);
    let (path, change) = match (planned.kind, &planned.source, &planned.target) {
        (Kind::Create, _, Some(target)) => (target.clone(), FileChangeKind::Created),
        (Kind::Delete, Some(source), _) => (source.clone(), FileChangeKind::Deleted),
        (Kind::Move, Some(source), Some(target)) => (
            target.clone(),
            FileChangeKind::Moved {
                from: source.clone(),
            },
        ),
        (_, _, target) => (target.clone().unwrap_or_default(), FileChangeKind::Modified),
    };
    let first_changed_line = match change {
        FileChangeKind::Deleted => None,
        _ => first_changed_line,
    };
    PatchedFile {
        path,
        change,
        diff,
        first_changed_line,
    }
}

fn summary(workspace: &Workspace, file: &PatchedFile) -> String {
    let path = workspace.display_path(&file.path);
    let (added, removed) = count_changes(&file.diff);
    match &file.change {
        FileChangeKind::Created => format!("created {path} (+{added})"),
        FileChangeKind::Deleted => format!("deleted {path}"),
        FileChangeKind::Modified => format!("modified {path} (+{added} -{removed})"),
        FileChangeKind::Moved { from } => format!(
            "moved {} to {path} (+{added} -{removed})",
            workspace.display_path(from)
        ),
    }
}

fn count_changes(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with('+') && !line.starts_with("+++") {
            (added + 1, removed)
        } else if line.starts_with('-') && !line.starts_with("---") {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".into(),
            description: "Change files in the workspace with a patch in unified diff format. One patch can change several files: each file starts with `--- a/<path>` and `+++ b/<path>` lines, then its hunks. `--- /dev/null` creates a file, `+++ /dev/null` deletes one, and two different paths move a file. Each hunk starts with an `@@ -<line>,<count> +<line>,<count> @@` line, and each of its lines starts with ' ' (unchanged), '-' (removed) or '+' (added). Hunks are placed by their unchanged and removed lines, which must match the file, so include about 3 unchanged lines around each change and keep a file's hunks in file order; line numbers only help choose between several matching places. Read a file before changing it. Nothing is changed unless the whole patch applies.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The patch, in unified diff format, for one or more files"
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput> {
        let args: EditArgs = parse_args(args)?;
        let files = parse_patch(&args.patch)?;
        ctx.ensure_not_cancelled()?;
        let workspace = ctx.workspace.clone();
        let planned = files
            .into_iter()
            .map(|file| plan(&workspace, file))
            .collect::<Result<Vec<_>>>()?;

        let mut paths: Vec<&Path> = Vec::new();
        for path in planned.iter().flat_map(Planned::paths) {
            if paths.contains(&path) {
                return Err(invalid(format!(
                    "{} appears more than once in the patch; put all of its hunks under one file header",
                    workspace.display_path(path)
                )));
            }
            paths.push(path);
        }
        let _guard = ctx.lock_files(&paths, FileAccess::Write).await;

        for planned in &planned {
            if let Some(source) = &planned.source {
                let display = workspace.display_path(source);
                let stamp = current_stamp(source)
                    .map_err(|err| ToolError::io(format!("cannot inspect {display}"), err))?;
                if stamp.is_none() {
                    return Err(ToolError::NotFound(PathBuf::from(display)));
                }
                ctx.files.ensure_fresh(source, stamp)?;
            }
            if let Some(target) = &planned.target
                && planned.kind != Kind::Modify
                && target.exists()
            {
                return Err(ToolError::failed(format!(
                    "{} already exists; change it with `--- a/<path>` and `+++ b/<path>` headers instead",
                    workspace.display_path(target)
                )));
            }
        }

        let outcome = tokio::task::spawn_blocking(move || {
            let prepared = planned
                .into_iter()
                .map(|planned| prepare(&workspace, planned))
                .collect::<Result<Vec<_>>>()
                .map_err(|err| (err, Written::default()))?;
            let mut written = Written::default();
            for (index, file) in prepared.iter().enumerate() {
                if let Err(err) = write_one(file, &mut written) {
                    let path = file
                        .planned
                        .target
                        .as_ref()
                        .or(file.planned.source.as_ref());
                    let display = path
                        .map(|path| workspace.display_path(path))
                        .unwrap_or_default();
                    let done: Vec<String> = prepared[..index]
                        .iter()
                        .map(|file| summary(&workspace, &patched_file(&workspace, file)))
                        .collect();
                    let message = if done.is_empty() {
                        format!("cannot write {display}")
                    } else {
                        format!(
                            "cannot write {display}, after the patch already {}",
                            done.join(", ")
                        )
                    };
                    return Err((ToolError::io(message, err), written));
                }
            }
            let files: Vec<PatchedFile> = prepared
                .iter()
                .map(|file| patched_file(&workspace, file))
                .collect();
            let summaries: Vec<String> =
                files.iter().map(|file| summary(&workspace, file)).collect();
            Ok((files, summaries, written))
        })
        .await
        .map_err(|err| ToolError::failed(format!("edit task failed: {err}")))?;

        let record = |written: Written| {
            for (path, stamp) in written.record {
                ctx.files.record(&path, stamp);
            }
            for path in written.forget {
                ctx.files.forget(&path);
            }
        };
        let (files, summaries, written) = match outcome {
            Ok(done) => done,
            Err((err, written)) => {
                record(written);
                return Err(err);
            }
        };
        record(written);
        Ok(ToolOutput {
            content: format!("Applied the patch: {}.", summaries.join(", ")),
            is_error: false,
            details: ToolDetails::Patch { files },
        })
    }
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

    async fn edit(ctx: &ToolContext, patch: &str) -> Result<ToolOutput> {
        EditTool.call(ctx, json!({ "patch": patch })).await
    }

    fn patched(output: ToolOutput) -> Vec<PatchedFile> {
        let ToolDetails::Patch { files } = output.details else {
            panic!("unexpected details");
        };
        files
    }

    #[test]
    fn finds_first_changed_line() {
        assert_eq!(first_changed_line("a\nb\nc\n", "a\nB\nc\n"), Some(2));
        assert_eq!(first_changed_line("a\n", "a\nb\n"), Some(2));
        assert_eq!(first_changed_line("a\n", "a\n"), None);
    }

    #[tokio::test]
    async fn edits_a_file_and_reports_its_diff() {
        let (dir, ctx) = context();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let patch = "--- a/main.rs\n+++ b/main.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"hi\");\n+    println!(\"안녕\");\n }\n";
        let output = edit(&ctx, patch).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    println!(\"안녕\");\n}\n"
        );
        assert_eq!(
            output.content,
            "Applied the patch: modified main.rs (+1 -1)."
        );
        let files = patched(output);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, path.canonicalize().unwrap());
        assert_eq!(files[0].change, FileChangeKind::Modified);
        assert_eq!(files[0].first_changed_line, Some(2));
        assert!(files[0].diff.contains("-    println!(\"hi\");"));
        assert!(files[0].diff.contains("+    println!(\"안녕\");"));
    }

    #[tokio::test]
    async fn changes_creates_deletes_and_moves_files_in_one_patch() {
        let (dir, ctx) = context();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(root.join("old.txt"), "keep\nme\n").unwrap();
        let patch = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
 one
-two
+2
--- /dev/null
+++ b/src/new.txt
@@ -0,0 +1,2 @@
+hello
+world
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
--- a/old.txt
+++ b/moved/new.txt
@@ -1,2 +1,2 @@
 keep
-me
+you
";
        let output = edit(&ctx, patch).await.unwrap();
        assert_eq!(
            output.content,
            "Applied the patch: modified a.txt (+1 -1), created src/new.txt (+2), deleted gone.txt, moved old.txt to moved/new.txt (+1 -1)."
        );
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\n2\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/new.txt")).unwrap(),
            "hello\nworld\n"
        );
        assert!(!root.join("gone.txt").exists());
        assert!(!root.join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("moved/new.txt")).unwrap(),
            "keep\nyou\n"
        );
        let changes: Vec<FileChangeKind> = patched(output).into_iter().map(|f| f.change).collect();
        assert_eq!(changes[0], FileChangeKind::Modified);
        assert_eq!(changes[1], FileChangeKind::Created);
        assert_eq!(changes[2], FileChangeKind::Deleted);
        assert!(matches!(changes[3], FileChangeKind::Moved { .. }));
    }

    #[tokio::test]
    async fn changes_nothing_unless_every_file_applies() {
        let (dir, ctx) = context();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        let patch =
            "--- a/a.txt\n+++ b/a.txt\n@@\n-one\n+1\n--- a/b.txt\n+++ b/b.txt\n@@\n-three\n+3\n";
        let error = edit(&ctx, patch).await.unwrap_err().to_string();
        assert!(error.contains("does not match b.txt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\n"
        );
    }

    #[tokio::test]
    async fn rejects_creating_an_existing_file_and_repeated_files() {
        let (dir, ctx) = context();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let error = edit(&ctx, "--- /dev/null\n+++ b/a.txt\n@@\n+x\n")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists"), "{error}");
        let patch =
            "--- a/a.txt\n+++ b/a.txt\n@@\n-one\n+1\n--- a/a.txt\n+++ b/a.txt\n@@\n-1\n+2\n";
        let error = edit(&ctx, patch).await.unwrap_err().to_string();
        assert!(error.contains("more than once"), "{error}");
    }

    #[tokio::test]
    async fn keeps_paths_whose_first_directory_is_a_or_b() {
        let (dir, ctx) = context();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        std::fs::write(dir.path().join("a/x.txt"), "one\n").unwrap();
        edit(&ctx, "--- a/x.txt\n+++ a/x.txt\n@@\n-one\n+1\n")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/x.txt")).unwrap(),
            "1\n"
        );
    }

    #[tokio::test]
    async fn preserves_crlf_and_bom() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "\u{feff}one\r\ntwo\r\n").unwrap();
        edit(&ctx, "--- a/a.txt\n+++ b/a.txt\n@@\n-one\n-two\n+1\n+2\n")
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
        let result = edit(&ctx, "--- a/a.txt\n+++ b/a.txt\n@@\n-one\n+1\n").await;
        assert!(matches!(result, Err(ToolError::StaleFile { .. })));
    }

    #[tokio::test]
    async fn allows_consecutive_edits_by_same_session() {
        let (dir, ctx) = context();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        for (old, new) in [("a", "A"), ("b", "B"), ("c", "C")] {
            let patch = format!("--- a/a.txt\n+++ b/a.txt\n@@\n-{old}\n+{new}\n");
            edit(&ctx, &patch).await.unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nB\nC\n");
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let (_dir, ctx) = context();
        let result = edit(&ctx, "--- a/missing.txt\n+++ b/missing.txt\n@@\n-a\n+b\n").await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn rejects_paths_outside_the_workspace() {
        let (_dir, ctx) = context();
        let result = edit(&ctx, "--- /dev/null\n+++ /etc/loom-test\n@@\n+x\n").await;
        assert!(matches!(result, Err(ToolError::OutsideWorkspace(_))));
    }
}
