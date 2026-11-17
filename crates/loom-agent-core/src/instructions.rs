use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use loom_config::AGENTS_MD;
use loom_proto::WorktreeInfo;
use loom_tools::ToolSpec;

use crate::environment::EnvSnapshot;
use crate::notes::NOTES_READ_TOOL;
use crate::skills::Catalog;
use crate::tasks::{self, TASKS_READ_TOOL};

pub const BUILTIN_INSTRUCTIONS: &str = "\
You are Loom, a coding agent working in a software project on the user's machine.

Work carefully and efficiently:
- Use the available tools to inspect files and run commands instead of guessing.
- Read a file before editing it, and keep edits minimal and focused on the request.
- Change files with the edit tool: one patch can change, create, delete, or move several files. Use the write tool
  to write a whole file. Do not change files with scripts or shell commands such as sed.
- Run builds and tests when they help verify a change, and report failures honestly.
- Do not run destructive commands unless the user asked for them.
- Keep responses concise. Show file paths clearly when referring to files.";

pub const COMMENTS_SECTION: &str = "\
# Comments

Do not add comments to code unless the user asks for them. The edit and write tools check each change for new comments. You may change existing comments and add lines next to them, and you may add tool directives such as `eslint-disable`.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt {
    pub text: String,
    pub warnings: Vec<String>,
}

pub struct PromptInputs<'a> {
    /// Replaces [`BUILTIN_INSTRUCTIONS`] when set.
    pub base: Option<&'a str>,
    pub system_agents_md: &'a Path,
    pub workspace_root: &'a Path,
    pub max_bytes: usize,
    pub tools: &'a [ToolSpec],
    /// The skills to list, if any.
    pub skills: Option<&'a Catalog>,
    /// The environment of commands, whose variable names the prompt lists
    /// and whose problems it reports.
    pub environment: Option<&'a EnvSnapshot>,
    /// The git worktree that the session works in, which the prompt names
    /// with its branch.
    pub worktree: Option<&'a WorktreeInfo>,
    /// The directories outside the workspace that the file tools may read
    /// and change.
    pub allowed_dirs: &'a [PathBuf],
    pub comment_guard: bool,
}

pub async fn build_system_prompt(inputs: &PromptInputs<'_>) -> SystemPrompt {
    let base = inputs.base.unwrap_or(BUILTIN_INSTRUCTIONS);
    let mut sections = vec![base.trim_end().to_owned()];
    let mut warnings = Vec::new();
    let workspace_md = inputs.workspace_root.join(AGENTS_MD);
    for (label, path) in [
        (
            "System-wide instructions",
            inputs.system_agents_md.to_path_buf(),
        ),
        ("Workspace instructions", workspace_md),
    ] {
        match read_capped(&path, inputs.max_bytes).await {
            Ok(Some((text, truncated))) => {
                if truncated {
                    warnings.push(format!(
                        "{} is larger than {} bytes and was truncated",
                        path.display(),
                        inputs.max_bytes
                    ));
                }
                if !text.trim().is_empty() {
                    sections.push(format!(
                        "# {label} ({})\n\n{}",
                        path.display(),
                        text.trim_end()
                    ));
                }
            }
            Ok(None) => {}
            Err(err) => warnings.push(format!("cannot read {}: {err}", path.display())),
        }
    }
    if let Some(catalog) = inputs.skills {
        warnings.extend(catalog.warnings.iter().cloned());
        if let Some(section) = catalog.prompt_section() {
            sections.push(section);
        }
    }
    if inputs
        .tools
        .iter()
        .any(|tool| tool.name == NOTES_READ_TOOL || tool.name == TASKS_READ_TOOL)
    {
        sections.push(tasks::PROMPT_SECTION.to_owned());
    }
    if inputs.comment_guard
        && inputs
            .tools
            .iter()
            .any(|tool| tool.name == "edit" || tool.name == "write")
    {
        sections.push(COMMENTS_SECTION.to_owned());
    }
    if let Some(environment) = inputs.environment {
        warnings.extend(environment.warnings.iter().cloned());
        if let Some(section) = variables_section(&environment.names) {
            sections.push(section);
        }
    }
    sections.push(runtime_section(
        inputs.workspace_root,
        inputs.worktree,
        inputs.allowed_dirs,
    ));
    SystemPrompt {
        text: sections.join("\n\n"),
        warnings,
    }
}

/// Lists the variables that the user set for commands, never their values.
fn variables_section(names: &[(String, bool)]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let list = |hidden: bool| {
        names
            .iter()
            .filter(|(_, h)| *h == hidden)
            .map(|(name, _)| format!("`{name}`"))
            .collect::<Vec<_>>()
    };
    let (secret, plain) = (list(true), list(false));
    let mut text =
        String::from("# Environment variables\n\nThe user set these variables for shell commands.");
    if !secret.is_empty() {
        text.push_str(&format!(
            "\n\n- Secret: {}. You cannot see their values: they show as `[REDACTED:<NAME>]` in tool output. Use them only as `$NAME` in commands, for example in the arguments of a CLI. Do not try to print, encode, or send their values anywhere.",
            secret.join(", ")
        ));
    }
    if !plain.is_empty() {
        text.push_str(&format!("\n- Not secret: {}.", plain.join(", ")));
    }
    text.push_str("\n- The env file holds their values. Only the user changes it.");
    Some(text)
}

async fn read_capped(path: &PathBuf, max_bytes: usize) -> std::io::Result<Option<(String, bool)>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let truncated = bytes.len() > max_bytes;
    let mut end = bytes.len().min(max_bytes);
    let text = loop {
        match std::str::from_utf8(&bytes[..end]) {
            Ok(text) => break text.to_owned(),
            Err(err) if truncated && err.error_len().is_none() => end = err.valid_up_to(),
            Err(_) => break String::from_utf8_lossy(&bytes[..end]).into_owned(),
        }
    };
    Ok(Some((text, truncated)))
}

fn runtime_section(
    workspace_root: &Path,
    worktree: Option<&WorktreeInfo>,
    allowed_dirs: &[PathBuf],
) -> String {
    let worktree = worktree
        .map(|w| {
            format!(
                "\n- Git worktree: the workspace is a git worktree on the branch `{}`, made from `{}`. Commit your changes on this branch. The user merges it or opens a pull request.",
                w.branch, w.base
            )
        })
        .unwrap_or_default();
    let allowed = if allowed_dirs.is_empty() {
        String::new()
    } else {
        let dirs: Vec<String> = allowed_dirs
            .iter()
            .map(|dir| format!("`{}`", dir.display()))
            .collect();
        format!(
            "\n- Allowed directories: {}. The read, edit, and write tools can also read and change files in these directories outside the workspace.",
            dirs.join(", ")
        )
    };
    format!(
        "# Environment\n\n- Date: {}\n- Workspace: {}{worktree}{allowed}\n- OS: {}",
        today(),
        workspace_root.display(),
        os_description(),
    )
}

fn os_description() -> String {
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        });
    match pretty {
        Some(name) => format!("{} ({})", std::env::consts::OS, name),
        None => std::env::consts::OS.to_owned(),
    }
}

fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02} (UTC)")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            parameters: json!({}),
        }
    }

    fn tools() -> Vec<ToolSpec> {
        vec![tool("read")]
    }

    #[tokio::test]
    async fn explains_notes_and_tasks_when_they_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "WORKSPACE RULE").unwrap();
        let missing = dir.path().join("missing.md");
        let prompt = |tools: Vec<ToolSpec>| {
            let dir = dir.path().to_path_buf();
            let missing = missing.clone();
            async move {
                build_system_prompt(&PromptInputs {
                    base: None,
                    system_agents_md: &missing,
                    workspace_root: &dir,
                    max_bytes: 1024,
                    tools: &tools,
                    skills: None,
                    environment: None,
                    worktree: None,
                    allowed_dirs: &[],
                    comment_guard: false,
                })
                .await
                .text
            }
        };
        let without = prompt(tools()).await;
        assert!(!without.contains("# Notes and tasks"));
        for read in [NOTES_READ_TOOL, TASKS_READ_TOOL] {
            let with = prompt(vec![tool("read"), tool(read)]).await;
            let ws = with.find("WORKSPACE RULE").unwrap();
            let section = with.find("# Notes and tasks").unwrap();
            let env = with.find("# Environment").unwrap();
            assert!(ws < section && section < env, "{read}");
        }
        assert!(!without.contains("# Comments"));
    }

    #[tokio::test]
    async fn explains_the_comment_guard_when_it_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.md");
        let prompt = |tools: Vec<ToolSpec>, comment_guard| {
            let dir = dir.path().to_path_buf();
            let missing = missing.clone();
            async move {
                build_system_prompt(&PromptInputs {
                    base: None,
                    system_agents_md: &missing,
                    workspace_root: &dir,
                    max_bytes: 1024,
                    tools: &tools,
                    skills: None,
                    environment: None,
                    worktree: None,
                    allowed_dirs: &[],
                    comment_guard,
                })
                .await
                .text
            }
        };
        assert!(
            prompt(vec![tool("edit")], true)
                .await
                .contains("# Comments")
        );
        assert!(
            !prompt(vec![tool("edit")], false)
                .await
                .contains("# Comments")
        );
        assert!(!prompt(tools(), true).await.contains("# Comments"));
    }

    #[test]
    fn converts_days_to_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[tokio::test]
    async fn assembles_sections_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("system.md");
        let workspace = dir.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(&system, "SYSTEM RULE").unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "WORKSPACE RULE").unwrap();
        let tools = tools();
        let catalog = Catalog {
            skills: vec![crate::skills::Skill {
                name: "pdf".into(),
                description: "PDF files.".into(),
                dir: dir.path().join("pdf"),
            }],
            warnings: vec!["skipped the skill in /x: no description".into()],
        };
        let environment = EnvSnapshot {
            names: vec![
                ("AWS_REGION".into(), false),
                ("LAKECTL_SECRET".into(), true),
            ],
            warnings: vec!["git does not ignore the env file .env".into()],
            ..EnvSnapshot::default()
        };
        let prompt = build_system_prompt(&PromptInputs {
            base: None,
            system_agents_md: &system,
            workspace_root: &workspace,
            max_bytes: 1024,
            tools: &tools,
            skills: Some(&catalog),
            environment: Some(&environment),
            worktree: Some(&WorktreeInfo {
                branch: "loom/abc".into(),
                base: "main".into(),
                path: "/w".into(),
                pull_request: None,
            }),
            allowed_dirs: &[PathBuf::from("/srv/notes"), PathBuf::from("/tmp/x")],
            comment_guard: false,
        })
        .await;
        let text = &prompt.text;
        let builtin = text.find("You are Loom").unwrap();
        let sys = text.find("SYSTEM RULE").unwrap();
        let ws = text.find("WORKSPACE RULE").unwrap();
        let skills = text.find("# Skills").unwrap();
        let vars = text.find("# Environment variables").unwrap();
        let env = text.find("# Environment\n").unwrap();
        assert!(builtin < sys && sys < ws && ws < skills && skills < vars && vars < env);
        assert!(text.contains("- Secret: `LAKECTL_SECRET`."));
        assert!(text.contains("- Not secret: `AWS_REGION`."));
        assert!(text.contains("\n- pdf: PDF files."));
        assert!(!text.contains("- Tools:"));
        assert!(text.contains("on the branch `loom/abc`, made from `main`"));
        assert!(text.contains("\n- Allowed directories: `/srv/notes`, `/tmp/x`. "));
        let mut warnings = catalog.warnings.clone();
        warnings.extend(environment.warnings.clone());
        assert_eq!(prompt.warnings, warnings);

        let empty = Catalog::default();
        let inputs = |skills| PromptInputs {
            base: None,
            system_agents_md: &system,
            workspace_root: &workspace,
            max_bytes: 1024,
            tools: &tools,
            skills,
            environment: None,
            worktree: None,
            allowed_dirs: &[],
            comment_guard: false,
        };
        let without = build_system_prompt(&inputs(Some(&empty))).await;
        assert!(!without.text.contains("# Skills"));
        assert!(without.warnings.is_empty());
        assert!(!without.text.contains("Allowed directories"));
    }

    #[tokio::test]
    async fn missing_files_are_skipped_and_large_files_truncated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "가나다라마바사").unwrap();
        let tools = tools();
        let prompt = build_system_prompt(&PromptInputs {
            base: None,
            system_agents_md: &dir.path().join("missing.md"),
            workspace_root: dir.path(),
            max_bytes: 7,
            tools: &tools,
            skills: None,
            environment: None,
            worktree: None,
            allowed_dirs: &[],
            comment_guard: false,
        })
        .await;
        assert!(prompt.text.contains("가나\n") || prompt.text.contains("가나\n\n"));
        assert!(!prompt.text.contains("다"));
        assert!(!prompt.text.contains("System-wide"));
        assert_eq!(prompt.warnings.len(), 1);
        assert!(prompt.warnings[0].contains("truncated"));
    }

    #[tokio::test]
    async fn configured_prompt_replaces_builtin_instructions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "WORKSPACE RULE").unwrap();
        let tools = tools();
        let prompt = build_system_prompt(&PromptInputs {
            base: Some("You are a careful reviewer.\n"),
            system_agents_md: &dir.path().join("missing.md"),
            workspace_root: dir.path(),
            max_bytes: 1024,
            tools: &tools,
            skills: None,
            environment: None,
            worktree: None,
            allowed_dirs: &[],
            comment_guard: false,
        })
        .await;
        let text = &prompt.text;
        assert!(text.starts_with("You are a careful reviewer.\n\n# Workspace"));
        assert!(!text.contains("You are Loom"));
        assert!(text.contains("WORKSPACE RULE"));
        assert!(text.contains("# Environment"));
    }
}
