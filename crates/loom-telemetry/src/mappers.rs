//! Pure helpers that turn Loom's tool calls and results into the attributes
//! Claude Code reports, after the pi-enhanced exporter.

use serde_json::{Map, Value};

/// Appended to text cut at `content_max_length`.
const TRUNCATION_MARKER: &str = "…[TRUNCATED";

/// The name Claude Code gives a code editing tool, for
/// `code_edit_tool.decision`.
pub fn code_edit_tool(name: &str) -> Option<&'static str> {
    match name {
        "edit" => Some("Edit"),
        "write" => Some("Write"),
        _ => None,
    }
}

pub fn is_shell(name: &str) -> bool {
    name == "shell"
}

pub fn string_arg<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

/// Added and removed lines in a unified diff, without the file headers.
pub fn diff_line_counts(diff: &str) -> (u64, u64) {
    let (mut added, mut removed) = (0, 0);
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// The simple commands of a shell command line, roughly.
fn commands(line: &str) -> impl Iterator<Item = &str> {
    line.split([';', '&', '|', '(', ')', '\n'])
}

fn git_subcommand(command: &str, subcommand: &str) -> bool {
    // `git [-c k=v | -C dir | --opt=value]... <subcommand>` in any of the
    // commands of a command line.
    commands(command).any(|part| {
        let words: Vec<&str> = part.split_whitespace().collect();
        words.iter().enumerate().any(|(i, word)| {
            if *word != "git" {
                return false;
            }
            let mut j = i + 1;
            while let Some(option) = words.get(j) {
                match *option {
                    "-c" | "-C" => j += 2,
                    w if w.starts_with("--") && w.contains('=') => j += 1,
                    _ => break,
                }
            }
            words.get(j) == Some(&subcommand)
        })
    })
}

/// Whether a shell command runs `git commit`. Like Claude Code, this counts
/// the command, not the commits it made.
pub fn creates_commit(command: &str) -> bool {
    git_subcommand(command, "commit")
}

/// Pull requests a shell command opens: `gh pr create` and `glab mr create`.
pub fn pull_requests_created(command: &str) -> u64 {
    let opens = |tool: &str, noun: &str| {
        commands(command).any(|part| {
            part.split_whitespace()
                .collect::<Vec<_>>()
                .windows(3)
                .any(|w| w[0] == tool && w[1] == noun && w[2] == "create")
        })
    };
    u64::from(opens("gh", "pr")) + u64::from(opens("glab", "mr"))
}

/// A stable, low-cardinality category for a failed tool result.
pub fn error_type(text: &str) -> String {
    let words = || {
        text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
    };
    if let Some(errno) = words().find(|w| {
        w.len() >= 4 && w.starts_with('E') && w[1..].chars().all(|c| c.is_ascii_uppercase())
    }) {
        return format!("Error:{errno}");
    }
    if let Some(named) = words().find(|w| {
        w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && (w.ends_with("Error") || w.ends_with("Exception"))
            && w.chars().all(|c| c.is_ascii_alphabetic())
    }) {
        return named.to_owned();
    }
    "Error".into()
}

/// Cuts `text` to `max` characters, ending with a marker that says how much
/// was removed.
pub fn truncate(text: &str, max: usize) -> String {
    let length = text.chars().count();
    if length <= max {
        return text.to_owned();
    }
    let marker = format!("{TRUNCATION_MARKER}: {} chars]", length - max);
    let marker_length = marker.chars().count();
    if marker_length >= max {
        return marker.chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - marker_length).collect();
    out.push_str(&marker);
    out
}

/// The tool arguments as JSON, with long strings shortened, for `tool_input`.
pub fn tool_input(arguments: &Value) -> String {
    const VALUE_LIMIT: usize = 512;
    const TOTAL_LIMIT: usize = 4096;
    fn shorten(value: &Value) -> Value {
        match value {
            Value::String(s) if s.chars().count() > VALUE_LIMIT => Value::String(format!(
                "{}…",
                s.chars().take(VALUE_LIMIT).collect::<String>()
            )),
            Value::Array(items) => Value::Array(items.iter().map(shorten).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), shorten(v)))
                    .collect::<Map<_, _>>(),
            ),
            other => other.clone(),
        }
    }
    let json = shorten(arguments).to_string();
    if json.chars().count() > TOTAL_LIMIT {
        format!("{}…", json.chars().take(TOTAL_LIMIT).collect::<String>())
    } else {
        json
    }
}

/// The tool-specific parameters Claude Code puts in `tool_parameters`.
pub fn tool_parameters(name: &str, arguments: &Value) -> Value {
    let mut params = Map::new();
    if is_shell(name) {
        let command = string_arg(arguments, "command");
        if let Some(command) = command {
            if let Some(program) = command.split_whitespace().next() {
                params.insert("bash_command".into(), program.into());
            }
            params.insert("full_command".into(), command.into());
            if creates_commit(command) {
                params.insert("git_commit".into(), true.into());
            }
        }
        if let Some(timeout) = arguments.get("timeout") {
            params.insert("timeout".into(), timeout.clone());
        }
    } else if let Some(path) = string_arg(arguments, "path") {
        params.insert("file_path".into(), path.into());
    }
    Value::Object(params)
}

/// The language of a source file, from its name, for code edit decisions.
pub fn language(path: &str) -> Option<&'static str> {
    let name = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" => return Some("Dockerfile"),
        "makefile" | "gnumakefile" => return Some("Makefile"),
        _ => {}
    }
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    Some(match extension {
        "rs" => "Rust",
        "py" | "pyi" => "Python",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TypeScript",
        "js" | "mjs" | "cjs" | "jsx" => "JavaScript",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "scala" => "Scala",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "json" | "jsonc" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "md" | "markdown" => "Markdown",
        "xml" => "XML",
        "lua" => "Lua",
        "dart" => "Dart",
        "ex" | "exs" => "Elixir",
        "hs" => "Haskell",
        "ml" | "mli" => "OCaml",
        "zig" => "Zig",
        "nix" => "Nix",
        "proto" => "Protocol Buffers",
        "tf" => "Terraform",
        "vue" => "Vue",
        "svelte" => "Svelte",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn counts_diff_lines() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n keep\n-old\n+new\n+more\n";
        assert_eq!(diff_line_counts(diff), (2, 1));
    }

    #[test]
    fn detects_commits_and_pull_requests() {
        assert!(creates_commit("git commit -m x"));
        assert!(creates_commit(
            "cargo fmt && git -c user.name=a commit -F -"
        ));
        assert!(creates_commit("git -C repo commit"));
        assert!(!creates_commit("git log --grep commit"));
        assert!(!creates_commit("echo git; commit"));
        assert_eq!(pull_requests_created("gh pr create --fill"), 1);
        assert_eq!(pull_requests_created("gh pr list"), 0);
        assert_eq!(pull_requests_created("glab mr create; gh pr create"), 2);
    }

    #[test]
    fn categorises_errors() {
        assert_eq!(error_type("open: ENOENT: no such file"), "Error:ENOENT");
        assert_eq!(error_type("TypeError: x is undefined"), "TypeError");
        assert_eq!(error_type("exit status 1"), "Error");
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("héllo", 5), "héllo");
        let cut = truncate(&"é".repeat(100), 40);
        assert_eq!(cut.chars().count(), 40);
        assert!(cut.ends_with("…[TRUNCATED: 60 chars]"), "{cut}");
        assert_eq!(truncate("abcdef", 3), "…[T");
    }

    #[test]
    fn describes_tool_parameters() {
        assert_eq!(
            tool_parameters(
                "shell",
                &json!({"command": "git commit -m x", "timeout": 30})
            ),
            json!({"bash_command": "git", "full_command": "git commit -m x", "git_commit": true, "timeout": 30})
        );
        assert_eq!(
            tool_parameters("edit", &json!({"path": "src/a.rs", "old": "x"})),
            json!({"file_path": "src/a.rs"})
        );
        let long = tool_input(&json!({"content": "x".repeat(600)}));
        assert!(long.len() < 600 && long.contains('…'), "{long}");
    }

    #[test]
    fn names_languages() {
        assert_eq!(language("crates/a/src/lib.rs"), Some("Rust"));
        assert_eq!(language("Dockerfile"), Some("Dockerfile"));
        assert_eq!(language(".gitignore"), None);
        assert_eq!(language("notes"), None);
    }
}
