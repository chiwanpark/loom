//! Unified-diff patches: parsing and applying them by their context.
//!
//! The parser is lenient where models often slip: hunk headers may lack line
//! numbers (`@@`), line counts may be wrong, and an empty context line may
//! lack its leading space. It is strict where a slip would silently lose a
//! change: every line inside a file's section has to belong to a hunk.
//!
//! Hunks are placed by their context and removed lines, not by their line
//! numbers. Line numbers only pick between several matching places. When the
//! lines do not match exactly, matching ignores trailing and then surrounding
//! whitespace; context lines keep the file's own text either way.

use crate::error::{Result, ToolError};

/// The text that names a missing side of a file patch, as in a created or a
/// deleted file.
pub const DEV_NULL: &str = "/dev/null";

const MAX_SHOWN_LINES: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The hunk's `@@` line, for messages.
    pub header: String,
    /// The 1-based first line of the hunk in the original text, if given.
    pub old_start: Option<usize>,
    pub lines: Vec<HunkLine>,
    /// The original text ends at the hunk's last old line, without a newline.
    pub old_no_newline: bool,
    /// The new text ends at the hunk's last new line, without a newline.
    pub new_no_newline: bool,
}

impl Hunk {
    fn old_lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().filter_map(|line| match line {
            HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
            HunkLine::Add(_) => None,
        })
    }

    fn has_old_lines(&self) -> bool {
        self.old_lines().next().is_some()
    }
}

/// The changes to one file. A missing path (`/dev/null` in the patch) means
/// the file is created or deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// The path after `---`, as written, without a timestamp.
    pub old_path: Option<String>,
    /// The path after `+++`, as written, without a timestamp.
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments(message.into())
}

fn patch_lines(patch: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = patch
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Parses a patch of one or more files, each starting with `---` and `+++`
/// lines. Git's extended header lines (`diff --git`, `index`, modes) are
/// allowed and ignored.
pub fn parse_patch(patch: &str) -> Result<Vec<FilePatch>> {
    let lines = patch_lines(patch);
    let mut files = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if is_file_header(&lines, index) {
            let old_path = header_path(&line[4..]);
            let new_path = header_path(&lines[index + 1][4..]);
            if old_path.is_none() && new_path.is_none() {
                return Err(invalid(format!(
                    "line {}: both sides of a file are {DEV_NULL}",
                    index + 1
                )));
            }
            index += 2;
            let mut hunks = Vec::new();
            while index < lines.len() && lines[index].starts_with("@@") {
                let (hunk, next) = parse_hunk(&lines, index)?;
                hunks.push(hunk);
                index = next;
            }
            if hunks.is_empty() && old_path.is_some() && new_path.is_some() {
                return Err(invalid(format!(
                    "line {}: the file {} has no hunks; each change starts with an @@ line",
                    index + 1,
                    new_path.as_deref().unwrap_or_default()
                )));
            }
            files.push(FilePatch {
                old_path,
                new_path,
                hunks,
            });
            if index < lines.len() && !may_start_file(lines[index]) {
                return Err(not_in_hunk(index, lines[index]));
            }
        } else if line.starts_with("@@") {
            return Err(invalid(format!(
                "line {}: a hunk comes before any file header; start each file with `--- a/<path>` and `+++ b/<path>` lines",
                index + 1
            )));
        } else if line.starts_with("*** Begin Patch") || line.starts_with("*** Update File") {
            return Err(invalid(
                "the patch must be a unified diff (`--- a/<path>`, `+++ b/<path>`, then @@ hunks), not the `*** Begin Patch` format",
            ));
        } else if files.is_empty() || may_start_file(line) {
            index += 1;
        } else {
            return Err(not_in_hunk(index, line));
        }
    }
    if files.is_empty() {
        return Err(invalid(
            "the patch has no files; start each file with `--- a/<path>` and `+++ b/<path>` lines, then its @@ hunks",
        ));
    }
    Ok(files)
}

/// Parses a patch of one text without file headers: only its hunks. File
/// headers are allowed when there is a single file, and are ignored.
pub fn parse_hunks(patch: &str) -> Result<Vec<Hunk>> {
    let lines = patch_lines(patch);
    let starts_with_header = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .is_some_and(|first| !lines[first].starts_with("@@"));
    if starts_with_header {
        let mut files = parse_patch(patch)?;
        if files.len() != 1 {
            return Err(invalid("the patch must change a single text"));
        }
        return Ok(files.remove(0).hunks);
    }
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].starts_with("@@") {
            let (hunk, next) = parse_hunk(&lines, index)?;
            hunks.push(hunk);
            index = next;
        } else if lines[index].trim().is_empty() {
            index += 1;
        } else {
            return Err(not_in_hunk(index, lines[index]));
        }
    }
    if hunks.is_empty() {
        return Err(invalid(
            "the patch has no hunks; each change starts with an @@ line",
        ));
    }
    Ok(hunks)
}

fn not_in_hunk(index: usize, line: &str) -> ToolError {
    invalid(format!(
        "line {} is not part of a hunk: {line:?}. Every line of a hunk starts with ' ' (unchanged), '-' (removed) or '+' (added)",
        index + 1
    ))
}

fn is_file_header(lines: &[&str], index: usize) -> bool {
    lines[index].starts_with("--- ")
        && lines
            .get(index + 1)
            .is_some_and(|next| next.starts_with("+++ "))
}

/// Lines that may appear between files: blank lines and git's extended
/// headers.
fn may_start_file(line: &str) -> bool {
    const GIT_HEADERS: &[&str] = &[
        "diff ",
        "index ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "dissimilarity index",
        "rename from",
        "rename to",
        "--- ",
    ];
    line.trim().is_empty() || GIT_HEADERS.iter().any(|header| line.starts_with(header))
}

fn header_path(text: &str) -> Option<String> {
    let text = text.split('\t').next().unwrap_or_default().trim();
    let path = unquote(text);
    (path != DEV_NULL).then_some(path)
}

fn unquote(text: &str) -> String {
    let Some(inner) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return text.to_owned();
    };
    let mut path = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            path.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => path.push('\n'),
            Some('t') => path.push('\t'),
            Some(other) => path.push(other),
            None => path.push('\\'),
        }
    }
    path
}

struct Counts {
    old: usize,
    new: usize,
}

/// Parses `@@ -a,b +c,d @@`; any part may be missing.
fn parse_range_header(header: &str) -> (Option<usize>, Option<Counts>) {
    let body = header.trim_start_matches('@').trim_start();
    let body = body.split("@@").next().unwrap_or_default();
    let mut old = None;
    let mut new = None;
    for part in body.split_whitespace() {
        let parse = |range: &str| {
            let mut parts = range.splitn(2, ',');
            let start = parts.next()?.parse::<usize>().ok()?;
            let count = match parts.next() {
                Some(count) => count.parse::<usize>().ok()?,
                None => 1,
            };
            Some((start, count))
        };
        if let Some(range) = part.strip_prefix('-') {
            old = parse(range);
        } else if let Some(range) = part.strip_prefix('+') {
            new = parse(range);
        }
    }
    let counts = match (old, new) {
        (Some((_, old)), Some((_, new))) => Some(Counts { old, new }),
        _ => None,
    };
    (old.map(|(start, _)| start), counts)
}

fn parse_hunk(lines: &[&str], start: usize) -> Result<(Hunk, usize)> {
    let header = lines[start].to_owned();
    let (old_start, counts) = parse_range_header(&header);
    let mut hunk_lines = Vec::new();
    // Indexes of empty lines taken as context lines that lack their space.
    let mut bare = Vec::new();
    let (mut old_seen, mut new_seen) = (0, 0);
    let mut old_no_newline = false;
    let mut new_no_newline = false;
    let mut index = start + 1;
    while index < lines.len() {
        let line = lines[index];
        let counted = counts
            .as_ref()
            .is_some_and(|counts| old_seen >= counts.old && new_seen >= counts.new);
        let header_ahead = is_file_header(lines, index) || line.starts_with("diff ");
        if line.starts_with("@@") || (header_ahead && (counts.is_none() || counted)) {
            break;
        }
        if let Some(text) = line.strip_prefix(' ') {
            hunk_lines.push(HunkLine::Context(text.to_owned()));
            old_seen += 1;
            new_seen += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            hunk_lines.push(HunkLine::Remove(text.to_owned()));
            old_seen += 1;
        } else if let Some(text) = line.strip_prefix('+') {
            hunk_lines.push(HunkLine::Add(text.to_owned()));
            new_seen += 1;
        } else if line.starts_with('\\') {
            match hunk_lines.last() {
                Some(HunkLine::Context(_)) => {
                    old_no_newline = true;
                    new_no_newline = true;
                }
                Some(HunkLine::Remove(_)) => old_no_newline = true,
                Some(HunkLine::Add(_)) => new_no_newline = true,
                None => {}
            }
        } else if line.is_empty() {
            bare.push(hunk_lines.len());
            hunk_lines.push(HunkLine::Context(String::new()));
            old_seen += 1;
            new_seen += 1;
        } else {
            break;
        }
        index += 1;
    }
    // Trailing bare empty lines more likely separate the hunk from what
    // follows than belong to it; dropping context is harmless.
    while bare
        .last()
        .is_some_and(|&last| last + 1 == hunk_lines.len())
    {
        bare.pop();
        hunk_lines.pop();
    }
    if !hunk_lines
        .iter()
        .any(|line| !matches!(line, HunkLine::Context(_)))
    {
        return Err(invalid(format!(
            "line {}: the hunk `{header}` changes nothing; mark removed lines with '-' and added lines with '+'",
            start + 1
        )));
    }
    Ok((
        Hunk {
            header,
            old_start,
            lines: hunk_lines,
            old_no_newline,
            new_no_newline,
        },
        index,
    ))
}

/// Ways of comparing a patch line with a file line, strictest first.
const MATCHERS: [fn(&str, &str) -> bool; 3] = [
    |a, b| a == b,
    |a, b| a.trim_end() == b.trim_end(),
    |a, b| a.trim() == b.trim(),
];

fn matches_at(lines: &[&str], at: usize, old: &[&str], same: fn(&str, &str) -> bool) -> bool {
    at + old.len() <= lines.len()
        && old
            .iter()
            .zip(&lines[at..])
            .all(|(expected, actual)| same(expected, actual))
}

/// Applies hunks, in order, to `content` (with `\n` line endings) and
/// returns the new text. `label` names the text in errors.
pub fn apply_hunks(content: &str, hunks: &[Hunk], label: &str) -> Result<String> {
    let mut ends_with_newline = content.is_empty() || content.ends_with('\n');
    let lines: Vec<&str> = if content.is_empty() {
        Vec::new()
    } else {
        content
            .strip_suffix('\n')
            .unwrap_or(content)
            .split('\n')
            .collect()
    };
    let mut output: Vec<&str> = Vec::with_capacity(lines.len());
    let mut cursor = 0;
    for (number, hunk) in hunks.iter().enumerate() {
        let name = if hunks.len() == 1 {
            format!("the hunk `{}`", hunk.header)
        } else {
            format!("hunk {} (`{}`)", number + 1, hunk.header)
        };
        let at = place(&lines, cursor, hunk, &name, label)?;
        output.extend_from_slice(&lines[cursor..at]);
        let mut old_index = at;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(_) => {
                    output.push(lines[old_index]);
                    old_index += 1;
                }
                HunkLine::Remove(_) => old_index += 1,
                HunkLine::Add(text) => output.push(text),
            }
        }
        cursor = old_index;
        if cursor == lines.len() {
            if hunk.new_no_newline {
                ends_with_newline = false;
            } else if hunk.old_no_newline {
                ends_with_newline = true;
            }
        }
    }
    output.extend_from_slice(&lines[cursor..]);
    let mut text = output.join("\n");
    if ends_with_newline && !output.is_empty() {
        text.push('\n');
    }
    if text == content {
        return Err(ToolError::failed(format!(
            "the patch makes no changes to {label}"
        )));
    }
    Ok(text)
}

/// Finds where a hunk's old lines start, at or after `cursor`.
fn place(lines: &[&str], cursor: usize, hunk: &Hunk, name: &str, label: &str) -> Result<usize> {
    let hint = hunk.old_start.map(|start| start.saturating_sub(1));
    if !hunk.has_old_lines() {
        // A pure insertion: `@@ -a,0 ...` inserts after line a.
        return match (hunk.old_start, lines.is_empty()) {
            (_, true) => Ok(0),
            (Some(after), false) if after >= cursor && after <= lines.len() => Ok(after),
            (Some(_), false) => Err(ToolError::failed(format!(
                "{name} inserts lines at a position outside {label} or before the previous hunk"
            ))),
            (None, false) => Err(ToolError::failed(format!(
                "{name} has no unchanged or removed lines, so its place in {label} is unknown; add a few unchanged lines around the change"
            ))),
        };
    }
    let old: Vec<&str> = hunk.old_lines().collect();
    for same in MATCHERS {
        let found: Vec<usize> = (cursor..lines.len())
            .filter(|&at| matches_at(lines, at, &old, same))
            .collect();
        match found.as_slice() {
            [] => continue,
            [at] => return Ok(*at),
            many => {
                if let Some(at) = hint.filter(|hint| many.contains(hint)) {
                    return Ok(at);
                }
                return Err(ToolError::failed(format!(
                    "{name} matches {} places in {label} (lines {}); add more unchanged lines around the change so it matches one place",
                    many.len(),
                    many.iter()
                        .take(5)
                        .map(|at| (at + 1).to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
    }
    let earlier = (0..cursor.min(lines.len())).any(|at| {
        MATCHERS
            .iter()
            .any(|same| matches_at(lines, at, &old, *same))
    });
    if earlier {
        return Err(ToolError::failed(format!(
            "{name} matches {label} only before the previous hunk; hunks must be in file order and must not overlap"
        )));
    }
    Err(ToolError::failed(format!(
        "{name} does not match {label}: these unchanged and removed lines were not found together{}:\n{}\nRead the file again and copy the lines exactly.",
        nearest_hint(lines, &old),
        shown(&old)
    )))
}

/// Says where the first old line of a failing hunk is, to help the model find
/// the mismatch.
fn nearest_hint(lines: &[&str], old: &[&str]) -> String {
    let Some(first) = old.first().filter(|line| !line.trim().is_empty()) else {
        return String::new();
    };
    let found: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == first.trim())
        .map(|(at, _)| at + 1)
        .take(3)
        .collect();
    match found.as_slice() {
        [] => String::new(),
        [line] => format!(" (the first of them is at line {line}, but the lines after it differ)"),
        lines => format!(
            " (the first of them is at lines {}, but the lines after it differ)",
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn shown(old: &[&str]) -> String {
    let mut text: String = old
        .iter()
        .take(MAX_SHOWN_LINES)
        .map(|line| format!("  {line}\n"))
        .collect();
    if old.len() > MAX_SHOWN_LINES {
        text.push_str(&format!(
            "  ... ({} more lines)\n",
            old.len() - MAX_SHOWN_LINES
        ));
    }
    text.pop();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(content: &str, patch: &str) -> Result<String> {
        apply_hunks(content, &parse_hunks(patch)?, "f")
    }

    #[test]
    fn applies_a_hunk_by_its_context() {
        let patch = "@@ -2,3 +2,3 @@\n b\n-c\n+C\n d\n";
        assert_eq!(apply("a\nb\nc\nd\ne\n", patch).unwrap(), "a\nb\nC\nd\ne\n");
    }

    #[test]
    fn ignores_wrong_or_missing_line_numbers() {
        let wrong = "@@ -40,3 +40,3 @@\n b\n-c\n+C\n d\n";
        assert_eq!(apply("a\nb\nc\nd\n", wrong).unwrap(), "a\nb\nC\nd\n");
        let bare = "@@\n b\n-c\n+C\n";
        assert_eq!(apply("a\nb\nc\nd\n", bare).unwrap(), "a\nb\nC\nd\n");
    }

    #[test]
    fn applies_several_hunks_in_order() {
        let patch = "@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -4,2 +4,3 @@\n d\n-e\n+E\n+F\n";
        assert_eq!(
            apply("a\nb\nc\nd\ne\n", patch).unwrap(),
            "A\nb\nc\nd\nE\nF\n"
        );
    }

    #[test]
    fn uses_line_numbers_to_choose_between_matches() {
        let content = "x\nfoo\ny\nfoo\nz\n";
        let patch = "@@ -4,1 +4,1 @@\n-foo\n+bar\n";
        assert_eq!(apply(content, patch).unwrap(), "x\nfoo\ny\nbar\nz\n");
        let error = apply(content, "@@\n-foo\n+bar\n").unwrap_err().to_string();
        assert!(error.contains("matches 2 places"), "{error}");
        assert!(error.contains("lines 2, 4"), "{error}");
    }

    #[test]
    fn rejects_out_of_order_and_unmatched_hunks() {
        let content = "a\nb\nc\n";
        let patch = "@@\n-c\n+C\n@@\n-a\n+A\n";
        let error = apply(content, patch).unwrap_err().to_string();
        assert!(error.contains("hunk 2"), "{error}");
        assert!(error.contains("before the previous hunk"), "{error}");

        let error = apply(content, "@@\n a\n-x\n+y\n").unwrap_err().to_string();
        assert!(error.contains("does not match f"), "{error}");
        assert!(error.contains("first of them is at line 1"), "{error}");
        assert!(error.contains("  x"), "{error}");
    }

    #[test]
    fn tolerates_whitespace_differences_and_keeps_the_files_context() {
        let content = "fn a() {   \n    one();\n}\n";
        let patch = "@@\n fn a() {\n-  one();\n+    two();\n }\n";
        assert_eq!(
            apply(content, patch).unwrap(),
            "fn a() {   \n    two();\n}\n"
        );
    }

    #[test]
    fn takes_bare_empty_lines_as_context() {
        let content = "a\n\nb\nc\n";
        let patch = "@@\n a\n\n-b\n+B\n\n";
        assert_eq!(apply(content, patch).unwrap(), "a\n\nB\nc\n");
    }

    #[test]
    fn handles_missing_newlines_at_the_end() {
        let patch = "@@\n-b\n\\ No newline at end of file\n+b\n";
        assert_eq!(apply("a\nb", patch).unwrap(), "a\nb\n");
        let patch = "@@\n-b\n+B\n\\ No newline at end of file\n";
        assert_eq!(apply("a\nb\n", patch).unwrap(), "a\nB");
        assert_eq!(apply("a\nb", "@@\n-b\n+B\n").unwrap(), "a\nB");
    }

    #[test]
    fn inserts_into_empty_text_and_by_line_number() {
        assert_eq!(apply("", "@@ -0,0 +1,2 @@\n+a\n+b\n").unwrap(), "a\nb\n");
        assert_eq!(
            apply("a\nc\n", "@@ -1,0 +2,1 @@\n+b\n").unwrap(),
            "a\nb\nc\n"
        );
        assert!(apply("a\n", "@@\n+b\n").is_err());
    }

    #[test]
    fn rejects_patches_that_change_nothing() {
        assert!(apply("a\n", "@@\n a\n").is_err());
        assert!(apply("a\n", "@@\n-a\n+a\n").is_err());
    }

    #[test]
    fn rejects_lines_outside_hunks() {
        let error = parse_hunks("@@\n a\n-b\n+c\nd\n").unwrap_err().to_string();
        assert!(error.contains("line 5"), "{error}");
        let patch = "--- a/x\n+++ b/x\n@@\n-a\n+b\noops\n";
        assert!(parse_patch(patch).is_err());
    }

    #[test]
    fn parses_multi_file_git_patches() {
        let patch = "diff --git a/src/x.rs b/src/x.rs\nindex 1..2 100644\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1 +1 @@\n-a\n+b\n\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\t2026-01-01\n@@ -0,0 +1 @@\n+hi\n--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n";
        let files = parse_patch(patch).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].old_path.as_deref(), Some("a/src/x.rs"));
        assert_eq!(files[0].new_path.as_deref(), Some("b/src/x.rs"));
        assert_eq!(files[1].old_path, None);
        assert_eq!(files[1].new_path.as_deref(), Some("b/new.txt"));
        assert_eq!(files[2].new_path, None);
        assert_eq!(
            files[2].hunks[0].lines,
            vec![HunkLine::Remove("bye".into())]
        );
    }

    #[test]
    fn uses_counts_to_tell_removed_dashes_from_headers() {
        let patch = "--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n--- old\n+++ new\n keep\n";
        let files = parse_patch(patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].hunks[0].lines,
            vec![
                HunkLine::Remove("-- old".into()),
                HunkLine::Add("++ new".into()),
                HunkLine::Context("keep".into()),
            ]
        );
    }

    #[test]
    fn rejects_other_formats_and_empty_patches() {
        let error = parse_patch("*** Begin Patch\n*** Update File: x\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unified diff"), "{error}");
        assert!(parse_patch("").is_err());
        assert!(parse_patch("@@\n-a\n+b\n").is_err());
        assert!(parse_patch("--- a/x\n+++ b/x\n").is_err());
        assert!(parse_hunks("").is_err());
    }

    #[test]
    fn parse_hunks_accepts_a_single_file_header() {
        let hunks = parse_hunks("--- a/notes\n+++ b/notes\n@@\n-a\n+b\n").unwrap();
        assert_eq!(hunks.len(), 1);
        assert!(
            parse_hunks("--- a/x\n+++ b/x\n@@\n-a\n+b\n--- a/y\n+++ b/y\n@@\n-a\n+b\n").is_err()
        );
    }

    #[test]
    fn unquotes_paths() {
        assert_eq!(
            header_path("\"b/with space\\\"q\"\t2026"),
            Some("b/with space\"q".into())
        );
        assert_eq!(header_path("/dev/null"), None);
    }
}
