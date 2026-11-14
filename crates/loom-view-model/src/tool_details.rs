//! How clients show a tool call's arguments and results: the shell command,
//! the content to write, or the edits as a diff, rather than raw JSON.

use loom_proto::ToolCall;

/// The most lines of arguments a client shows; the rest are counted.
pub const MAX_BLOCK_LINES: usize = 400;

/// The language of a file, by its name, for syntax highlighting: an
/// extension such as `rs`, or a name such as `make`.
pub fn language(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_lowercase();
    match lower.as_str() {
        "makefile" | "gnumakefile" => return "make".into(),
        "dockerfile" => return "dockerfile".into(),
        "cargo.lock" => return "toml".into(),
        _ => {}
    }
    let ext = match lower.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => return "text".into(),
    };
    match ext {
        "h" => "c",
        "hpp" | "hh" | "cc" | "cxx" => "cpp",
        "jsx" | "mjs" | "cjs" => "javascript",
        "mts" | "cts" => "typescript",
        "zsh" | "bash" => "bash",
        "htm" => "html",
        "txt" | "log" => "text",
        other => other,
    }
    .to_string()
}

fn pretty_arguments(arguments: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| arguments.to_owned()),
        Err(_) => arguments.to_owned(),
    }
}

/// What to show for a tool call's arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum CallDetails {
    None,
    Code { text: String, language: String },
    Diff(String),
}

/// Shows each tool's arguments in the form that says the most: the shell
/// command, the content to write, or the edits as a diff. The path is already
/// in the header, and the result's diff replaces the arguments of edits and
/// overwrites once it arrives.
pub fn call_details(call: &ToolCall, has_result_diff: bool) -> CallDetails {
    let args = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
    let field = |name: &str| {
        args.as_ref()
            .and_then(|args| args.get(name))
            .and_then(|value| value.as_str())
    };
    let code = |text: &str, language: &str| CallDetails::Code {
        text: text.to_owned(),
        language: language.to_owned(),
    };
    let long = |content: &str, language: &str| {
        let (shown, hidden) = head(content, MAX_BLOCK_LINES);
        let mut text = shown.to_owned();
        if hidden > 0 {
            text.push_str(&format!("\n… {hidden} more lines not shown"));
        }
        code(&text, language)
    };
    match call.name.as_str() {
        "shell" => {
            if let Some(command) = field("command") {
                return code(command, "bash");
            }
        }
        "read" if field("path").is_some() => return CallDetails::None,
        "search_web" if field("query").is_some() => return CallDetails::None,
        "edit" if has_result_diff => return CallDetails::None,
        "edit" => {
            if let Some(diff) = args.as_ref().and_then(edit_preview) {
                return CallDetails::Diff(diff);
            }
        }
        "write" if has_result_diff => return CallDetails::None,
        "write" => {
            if let (Some(path), Some(content)) = (field("path"), field("content")) {
                return long(content, &language(path));
            }
        }
        "notes_read" | "tasks_read" => return CallDetails::None,
        "notes_edit" => {
            if let Some(diff) = args.as_ref().and_then(edit_preview) {
                return CallDetails::Diff(diff);
            }
        }
        "notes_write" => {
            if let Some(text) = field("text").or_else(|| field("content")) {
                return long(text, "text");
            }
        }
        "tasks_update" => {
            if let Some(text) = args.as_ref().and_then(task_changes_preview) {
                return long(&text, "text");
            }
        }
        _ => {}
    }
    if args.is_some() {
        code(&pretty_arguments(&call.arguments), "json")
    } else {
        code(&call.arguments, "text")
    }
}

/// The changes of a tasks_update call, one per line: `+ text` adds a task,
/// `#3 → done` or `#3: text` updates one, `- #3` removes one.
pub fn task_changes_preview(args: &serde_json::Value) -> Option<String> {
    let changes = args.get("changes")?.as_array()?;
    let mut lines = Vec::new();
    for change in changes {
        let field = |name: &str| change.get(name).and_then(|v| v.as_str());
        let id = match change.get("id") {
            Some(serde_json::Value::Number(n)) => format!("#{n}"),
            Some(serde_json::Value::String(s)) => format!("#{}", s.trim_start_matches('#')),
            _ => "#?".to_owned(),
        };
        let text = field("text").map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "));
        let status = field("status");
        lines.push(match field("action")? {
            "add" => {
                let text = text.unwrap_or_default();
                match status {
                    Some(status) if status != "created" => format!("+ {text} ({status})"),
                    _ => format!("+ {text}"),
                }
            }
            "update" => match (text, status) {
                (Some(text), Some(status)) => format!("{id} → {status}: {text}"),
                (Some(text), None) => format!("{id}: {text}"),
                (None, Some(status)) => format!("{id} → {status}"),
                (None, None) => id,
            },
            "remove" => format!("- {id}"),
            other => format!("{other} {id}"),
        });
    }
    Some(lines.join("\n") + "\n")
}

/// The replacements of an edit call as a diff: removed lines, then added.
pub fn edit_preview(args: &serde_json::Value) -> Option<String> {
    let text = |value: &serde_json::Value, snake: &str, camel: &str| {
        value
            .get(snake)
            .or_else(|| value.get(camel))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let mut pairs: Vec<(String, String)> = args
        .get("edits")
        .and_then(|edits| edits.as_array())
        .into_iter()
        .flatten()
        .filter_map(|edit| {
            Some((
                text(edit, "old_text", "oldText")?,
                text(edit, "new_text", "newText")?,
            ))
        })
        .collect();
    if let (Some(old), Some(new)) = (
        text(args, "old_text", "oldText"),
        text(args, "new_text", "newText"),
    ) {
        pairs.push((old, new));
    }
    if pairs.is_empty() {
        return None;
    }
    let count = pairs.len();
    let mut out = Vec::new();
    for (ix, (old, new)) in pairs.iter().enumerate() {
        if count > 1 {
            out.push(format!("@@ edit {} of {count} @@", ix + 1));
        }
        out.extend(old.lines().map(|line| format!("-{line}")));
        out.extend(new.lines().map(|line| format!("+{line}")));
    }
    Some(out.join("\n"))
}

/// Splits the read tool's trailing note ("[Showing lines …]", "[File is
/// empty]", "[Line N is …]") from the file content.
pub fn split_read_note(content: &str) -> (&str, Option<&str>) {
    let content = content.trim_end_matches('\n');
    let (text, last) = match content.rsplit_once('\n') {
        Some((text, last)) => (text, last),
        None => ("", content),
    };
    let is_note = last.ends_with(']')
        && ["[Showing lines ", "[File is empty]", "[Line "]
            .iter()
            .any(|prefix| last.starts_with(prefix));
    if is_note {
        (text.trim_end_matches('\n'), Some(last))
    } else {
        (content, None)
    }
}

/// The first `max` lines of `text`, and how many lines were left out.
pub fn head(text: &str, max: usize) -> (&str, usize) {
    let total = text.lines().count();
    if total <= max {
        return (text, 0);
    }
    let end = text
        .match_indices('\n')
        .nth(max - 1)
        .map_or(text.len(), |(ix, _)| ix);
    (&text[..end], total - max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".into(),
            name: name.into(),
            arguments: args.to_string(),
            risk: None,
        }
    }

    fn code(text: &str, language: &str) -> CallDetails {
        CallDetails::Code {
            text: text.into(),
            language: language.into(),
        }
    }

    #[test]
    fn shell_shows_the_command() {
        let shell = call(
            "shell",
            serde_json::json!({"command": "cd /x && ls\necho hi", "timeout": 5}),
        );
        assert_eq!(
            call_details(&shell, false),
            code("cd /x && ls\necho hi", "bash")
        );
    }

    #[test]
    fn edit_shows_a_diff_until_the_result_has_one() {
        let edit = call(
            "edit",
            serde_json::json!({"path": "a.rs", "edits": [
                {"old_text": "a\nb", "new_text": "c"},
                {"oldText": "x", "newText": "y\nz"},
            ]}),
        );
        assert_eq!(call_details(&edit, true), CallDetails::None);
        assert_eq!(
            call_details(&edit, false),
            CallDetails::Diff(
                "@@ edit 1 of 2 @@\n-a\n-b\n+c\n@@ edit 2 of 2 @@\n-x\n+y\n+z".into()
            )
        );
        let single = serde_json::json!({"path": "a.rs", "old_text": "a", "new_text": "b"});
        assert_eq!(edit_preview(&single).as_deref(), Some("-a\n+b"));
    }

    #[test]
    fn write_shows_highlighted_content_unless_there_is_a_diff() {
        let write = call(
            "write",
            serde_json::json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
        );
        assert_eq!(call_details(&write, false), code("fn main() {}\n", "rs"));
        assert_eq!(call_details(&write, true), CallDetails::None);
    }

    #[test]
    fn task_updates_show_their_changes() {
        let read = call("tasks_read", serde_json::json!({}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let update = call(
            "tasks_update",
            serde_json::json!({"changes": [
                {"action": "add", "text": "build\n it"},
                {"action": "add", "text": "test", "status": "in_progress"},
                {"action": "update", "id": 1, "status": "done"},
                {"action": "update", "id": "2", "text": "test more"},
                {"action": "remove", "id": 3},
            ]}),
        );
        assert_eq!(
            call_details(&update, false),
            code(
                "+ build it\n+ test (in_progress)\n#1 → done\n#2: test more\n- #3\n",
                "text"
            )
        );
    }

    #[test]
    fn notes_calls_show_their_text_or_a_diff() {
        let read = call("notes_read", serde_json::json!({}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let edit = call(
            "notes_edit",
            serde_json::json!({"edits": [{"old_text": "- [ ] a", "new_text": "- [x] a"}]}),
        );
        assert_eq!(
            call_details(&edit, false),
            CallDetails::Diff("-- [ ] a\n+- [x] a".into())
        );
        let write = call("notes_write", serde_json::json!({"text": "# Plan\n"}));
        assert_eq!(call_details(&write, false), code("# Plan\n", "text"));
        let long = "line\n".repeat(MAX_BLOCK_LINES + 3);
        let write = call("notes_write", serde_json::json!({"content": long}));
        let CallDetails::Code { text, .. } = call_details(&write, false) else {
            panic!("no code block");
        };
        assert!(text.ends_with("… 3 more lines not shown"), "{text}");
    }

    #[test]
    fn read_has_no_argument_block_and_unknown_tools_show_json() {
        let read = call("read", serde_json::json!({"path": "a.rs", "offset": 3}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let search = call(
            "search_web",
            serde_json::json!({"query": "x", "maxSources": 3}),
        );
        assert_eq!(call_details(&search, false), CallDetails::None);
        let other = call("search", serde_json::json!({"q": "x"}));
        assert_eq!(
            call_details(&other, false),
            code("{\n  \"q\": \"x\"\n}", "json")
        );
    }

    #[test]
    fn read_notes_are_split_from_the_content() {
        assert_eq!(
            split_read_note("a\nb\n\n[Showing lines 1-2 of 9. Use offset=3 to continue.]"),
            (
                "a\nb",
                Some("[Showing lines 1-2 of 9. Use offset=3 to continue.]")
            )
        );
        assert_eq!(
            split_read_note("[File is empty]"),
            ("", Some("[File is empty]"))
        );
        // A TOML file can end with a table header; it is content, not a note.
        assert_eq!(
            split_read_note("a = 1\n[workspace]\n"),
            ("a = 1\n[workspace]", None)
        );
    }

    #[test]
    fn head_keeps_the_first_lines() {
        assert_eq!(head("a\nb\nc\n", 2), ("a\nb", 1));
        assert_eq!(head("a\nb", 2), ("a\nb", 0));
    }
}
