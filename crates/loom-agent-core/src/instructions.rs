use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use loom_config::AGENTS_MD;
use loom_tools::ToolSpec;

use crate::thread::{self, THREAD_READ_TOOL};

pub const BUILTIN_INSTRUCTIONS: &str = "\
You are Loom, a coding agent working in a software project on the user's machine.

Work carefully and efficiently:
- Use the available tools to inspect files and run commands instead of guessing.
- Read a file before editing it, and keep edits minimal and focused on the request.
- Prefer the edit tool for changes to existing files and the write tool for new files.
- Run builds and tests when they help verify a change, and report failures honestly.
- Do not run destructive commands unless the user asked for them.
- Keep responses concise. Show file paths clearly when referring to files.";

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
    if inputs
        .tools
        .iter()
        .any(|tool| tool.name == THREAD_READ_TOOL)
    {
        sections.push(thread::PROMPT_SECTION.to_owned());
    }
    sections.push(runtime_section(inputs.workspace_root, inputs.tools));
    SystemPrompt {
        text: sections.join("\n\n"),
        warnings,
    }
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

fn runtime_section(workspace_root: &Path, tools: &[ToolSpec]) -> String {
    let tools: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    format!(
        "# Environment\n\n- Date: {}\n- Workspace: {}\n- OS: {}\n- Tools: {}",
        today(),
        workspace_root.display(),
        os_description(),
        tools.join(", ")
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
    async fn explains_the_thread_when_it_can_be_read() {
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
                })
                .await
                .text
            }
        };
        let without = prompt(tools()).await;
        assert!(!without.contains("# Operation thread"));
        let with = prompt(vec![tool("read"), tool(THREAD_READ_TOOL)]).await;
        let ws = with.find("WORKSPACE RULE").unwrap();
        let thread = with.find("# Operation thread").unwrap();
        let env = with.find("# Environment").unwrap();
        assert!(ws < thread && thread < env);
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
        let prompt = build_system_prompt(&PromptInputs {
            base: None,
            system_agents_md: &system,
            workspace_root: &workspace,
            max_bytes: 1024,
            tools: &tools,
        })
        .await;
        let text = &prompt.text;
        let builtin = text.find("You are Loom").unwrap();
        let sys = text.find("SYSTEM RULE").unwrap();
        let ws = text.find("WORKSPACE RULE").unwrap();
        let env = text.find("# Environment").unwrap();
        assert!(builtin < sys && sys < ws && ws < env);
        assert!(text.contains("- Tools: read"));
        assert!(prompt.warnings.is_empty());
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
        })
        .await;
        let text = &prompt.text;
        assert!(text.starts_with("You are a careful reviewer.\n\n# Workspace"));
        assert!(!text.contains("You are Loom"));
        assert!(text.contains("WORKSPACE RULE"));
        assert!(text.contains("# Environment"));
    }
}
