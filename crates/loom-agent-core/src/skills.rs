//! Skills in the Agent Skills format (<https://agentskills.io>): directories
//! with a `SKILL.md` file, whose YAML frontmatter has the skill's `name` and
//! `description`, and whose body has its instructions. The system prompt
//! lists the skills, and the model loads one with the `skill` tool when a
//! task needs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use loom_config::{Dirs, SkillsConfig};
use loom_tools::{Risk, Tool, ToolContext, ToolDetails, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

pub const SKILL_TOOL: &str = "skill";
pub const SKILL_FILE: &str = "SKILL.md";
/// The start of a skill's instructions in a tool result. Compaction keeps
/// these results.
pub(crate) const CONTENT_START: &str = "<skill_content name=\"";
pub(crate) const CONTENT_END: &str = "</skill_content>";
/// The most files of a skill that its instructions list.
const MAX_LISTED_FILES: usize = 100;
const MAX_LIST_DEPTH: usize = 4;
/// Directories that tools make, which the list of a skill's files leaves
/// out. Hidden directories, such as `.venv`, are left out too.
const GENERATED_DIRS: &[&str] = &["__pycache__", "node_modules"];
/// The file in the root of a Python virtual environment, such as `venv`.
const VENV_MARKER: &str = "pyvenv.cfg";
/// Endings of files that tools make, such as compiled Python modules.
const GENERATED_FILE_ENDINGS: &[&str] = &[".pyc", ".pyo", "~"];
/// Files that tools make, by name.
const GENERATED_FILES: &[&str] = &["Thumbs.db"];
/// The most of a `SKILL.md` file that discovery reads for its frontmatter.
const MAX_SKILL_FILE: u64 = 256 * 1024;

const PROMPT_INTRO: &str = "# Skills\n\n\
Skills are instructions for special tasks. When a task matches the description of a skill, \
call the skill tool with the skill's name before you start the task, and then follow the \
instructions that it returns.";

/// Where an agent looks for skills.
#[derive(Debug, Clone)]
pub struct SkillSources {
    config: SkillsConfig,
    user_dirs: Vec<PathBuf>,
}

impl SkillSources {
    pub fn new(config: &SkillsConfig, dirs: &Dirs) -> Self {
        Self {
            config: config.clone(),
            user_dirs: vec![
                dirs.config_dir().join("skills"),
                dirs.home.join(".agents/skills"),
            ],
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// The directories of skills, the first ones winning over later ones
    /// with skills of the same name.
    fn roots(&self, workspace: &Path) -> Vec<PathBuf> {
        let mut roots = vec![
            workspace.join(".loom/skills"),
            workspace.join(".agents/skills"),
        ];
        roots.extend(self.user_dirs.iter().cloned());
        roots.extend(self.config.paths.iter().map(|path| workspace.join(path)));
        roots
    }

    /// The skills for a session in `workspace`. Reads the directories again
    /// each time, so that new and changed skills apply at once.
    pub fn discover(&self, workspace: &Path) -> Catalog {
        let mut catalog = Catalog::default();
        if !self.config.enabled {
            return catalog;
        }
        let mut found: BTreeMap<String, Skill> = BTreeMap::new();
        for root in self.roots(workspace) {
            for dir in skill_dirs(&root) {
                match load(&dir) {
                    Ok(skill) => {
                        if self.config.disabled.contains(&skill.name) {
                            continue;
                        }
                        found.entry(skill.name.clone()).or_insert(skill);
                    }
                    Err(message) => catalog
                        .warnings
                        .push(format!("skipped the skill in {}: {message}", dir.display())),
                }
            }
        }
        catalog.skills = found.into_values().collect();
        catalog
    }
}

/// The directories with a `SKILL.md` in `root`: `root` itself when it is
/// one skill, else its subdirectories.
fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    if root.join(SKILL_FILE).is_file() {
        return vec![root.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(SKILL_FILE).is_file())
        .collect();
    dirs.sort();
    dirs
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// By name.
    pub skills: Vec<Skill>,
    /// The skills that could not be loaded, and why.
    pub warnings: Vec<String>,
}

impl Catalog {
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The directories of the skills, which the read tool may read.
    pub fn dirs(&self) -> Vec<PathBuf> {
        self.skills.iter().map(|skill| skill.dir.clone()).collect()
    }

    /// The section of the system prompt that lists the skills, if any.
    pub fn prompt_section(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut section = format!("{PROMPT_INTRO}\n");
        for skill in &self.skills {
            let description = skill.description.split_whitespace().collect::<Vec<_>>();
            section.push_str(&format!("\n- {}: {}", skill.name, description.join(" ")));
        }
        Some(section)
    }
}

#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

fn load(dir: &Path) -> Result<Skill, String> {
    use std::io::Read;

    let path = dir.join(SKILL_FILE);
    let mut text = String::new();
    std::fs::File::open(&path)
        .and_then(|file| file.take(MAX_SKILL_FILE).read_to_string(&mut text))
        .map_err(|err| format!("cannot read {SKILL_FILE}: {err}"))?;
    let (frontmatter, _) = split(&text)?;
    let description = frontmatter
        .description
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty())
        .ok_or("its frontmatter has no description")?;
    let name = frontmatter
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .or_else(|| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or("it has no name")?;
    Ok(Skill {
        name,
        description,
        // Canonical, as the read tool compares the paths it reads with it.
        dir: dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
    })
}

/// The frontmatter and the body of a `SKILL.md` file.
fn split(text: &str) -> Result<(Frontmatter, &str), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "---" {
        return Err(format!("{SKILL_FILE} does not start with YAML frontmatter"));
    }
    let start = first.len();
    let mut end = start;
    for line in lines {
        if line.trim_end() == "---" {
            let yaml = &text[start..end];
            let body = &text[end + line.len()..];
            return Ok((parse_frontmatter(yaml)?, body.trim()));
        }
        end += line.len();
    }
    Err(format!("the frontmatter of {SKILL_FILE} has no end"))
}

/// Parses the frontmatter as YAML, or else reads its top-level `key: value`
/// lines, as other agents accept values with colons that YAML does not.
fn parse_frontmatter(yaml: &str) -> Result<Frontmatter, String> {
    if yaml.trim().is_empty() {
        return Ok(Frontmatter::default());
    }
    if let Ok(frontmatter) = serde_saphyr::from_str::<Frontmatter>(yaml) {
        return Ok(frontmatter);
    }
    let mut frontmatter = Frontmatter::default();
    for line in yaml.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim()).to_owned();
        match key.trim() {
            "name" => frontmatter.name = Some(value),
            "description" => frontmatter.description = Some(value),
            _ => {}
        }
    }
    if frontmatter.name.is_none() && frontmatter.description.is_none() {
        return Err("its frontmatter is not valid YAML".into());
    }
    Ok(frontmatter)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|v| v.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Whether a skill's file or directory is something that tools made, such
/// as `__pycache__`, so that the list of its files leaves it out.
fn generated(path: &Path, is_dir: bool) -> bool {
    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return false;
    };
    if name.starts_with('.') {
        return true;
    }
    if is_dir {
        GENERATED_DIRS.contains(&name.as_ref()) || path.join(VENV_MARKER).is_file()
    } else {
        GENERATED_FILES.contains(&name.as_ref())
            || GENERATED_FILE_ENDINGS.iter().any(|end| name.ends_with(end))
    }
}

/// The files of a skill besides `SKILL.md` and files that tools made,
/// relative to its directory, and whether there are more than the list holds.
fn list_files(dir: &Path) -> (Vec<String>, bool) {
    fn walk(base: &Path, dir: &Path, depth: usize, files: &mut Vec<String>) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            let is_dir = path.is_dir();
            if generated(&path, is_dir) {
                continue;
            }
            if is_dir {
                if depth < MAX_LIST_DEPTH && walk(base, &path, depth + 1, files) {
                    return true;
                }
                continue;
            }
            let Ok(relative) = path.strip_prefix(base) else {
                continue;
            };
            if relative == Path::new(SKILL_FILE) {
                continue;
            }
            if files.len() == MAX_LISTED_FILES {
                return true;
            }
            files.push(relative.display().to_string());
        }
        false
    }
    let mut files = Vec::new();
    let more = walk(dir, dir, 1, &mut files);
    (files, more)
}

/// Reads up to `max_bytes` of a text file, and says whether it was longer.
fn read_text(path: &Path, max_bytes: usize) -> Result<(String, bool), ToolError> {
    let bytes = std::fs::read(path)
        .map_err(|err| ToolError::io(format!("cannot read {}", path.display()), err))?;
    let truncated = bytes.len() > max_bytes;
    let mut end = bytes.len().min(max_bytes);
    loop {
        match std::str::from_utf8(&bytes[..end]) {
            Ok(text) => return Ok((text.to_owned(), truncated)),
            Err(err) if truncated && err.error_len().is_none() => end = err.valid_up_to(),
            Err(_) => {
                return Err(ToolError::failed(format!(
                    "{} is not a text file",
                    path.display()
                )));
            }
        }
    }
}

/// The skill's instructions as the model gets them, with its directory and
/// its files.
fn instructions(skill: &Skill, max_bytes: usize) -> Result<String, ToolError> {
    let (text, truncated) = read_text(&skill.dir.join(SKILL_FILE), max_bytes)?;
    let body = split(&text)
        .map(|(_, body)| body.to_owned())
        .unwrap_or(text);
    let mut out = format!("{CONTENT_START}{}\">\n{body}\n", skill.name);
    if truncated {
        out.push_str(&format!(
            "\n[{SKILL_FILE} is larger than {max_bytes} bytes; the rest was cut off]\n"
        ));
    }
    out.push_str(&format!(
        "\nSkill directory: {}\nRelative paths in this skill are relative to the skill directory; \
         use absolute paths in tool calls. Read its files with the read tool, and run its scripts \
         with the shell tool.\n",
        skill.dir.display()
    ));
    let (files, more) = list_files(&skill.dir);
    if !files.is_empty() {
        out.push_str("\n<skill_files>\n");
        for file in files {
            out.push_str(&file);
            out.push('\n');
        }
        if more {
            out.push_str("[more files are not listed]\n");
        }
        out.push_str("</skill_files>\n");
    }
    out.push_str(CONTENT_END);
    Ok(out)
}

/// Loads a skill's instructions, with its directory and its files.
pub struct SkillTool {
    sources: Arc<SkillSources>,
    max_bytes: usize,
}

impl SkillTool {
    pub fn new(sources: Arc<SkillSources>, max_bytes: usize) -> Self {
        Self { sources, max_bytes }
    }
}

#[derive(Deserialize)]
struct SkillArgs {
    name: String,
}

#[async_trait]
impl Tool for SkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SKILL_TOOL.into(),
            description: "Load a skill from the list of skills in the system prompt: its instructions, its directory, and its files. Read the files with the read tool when the instructions need them.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The name of the skill"
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: SkillArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
        let sources = self.sources.clone();
        let workspace = ctx.workspace.root().to_path_buf();
        let max_bytes = self.max_bytes;
        let name = args.name.trim().to_owned();
        tokio::task::spawn_blocking(move || {
            let catalog = sources.discover(&workspace);
            let skill = catalog.get(&name).ok_or_else(|| {
                let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
                ToolError::failed(if names.is_empty() {
                    format!("no skill named {name:?}; there are no skills")
                } else {
                    format!(
                        "no skill named {name:?}; the skills are {}",
                        names.join(", ")
                    )
                })
            })?;
            Ok(ToolOutput {
                content: instructions(skill, max_bytes)?,
                is_error: false,
                details: ToolDetails::Skill {
                    name: skill.name.clone(),
                },
            })
        })
        .await
        .map_err(|err| ToolError::failed(err.to_string()))?
    }
}

/// The skills' instructions in `texts`, by name, the last one of each name
/// winning. Compaction carries them over into the summary.
pub(crate) fn loaded_instructions<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut found: Vec<(String, String)> = Vec::new();
    for text in texts {
        let mut rest = text;
        while let Some(start) = rest.find(CONTENT_START) {
            let block = &rest[start..];
            let Some(end) = block.find(CONTENT_END) else {
                break;
            };
            let content = &block[..end + CONTENT_END.len()];
            let name = block[CONTENT_START.len()..]
                .split('"')
                .next()
                .unwrap_or_default()
                .to_owned();
            found.retain(|(other, _)| *other != name);
            found.push((name, content.to_owned()));
            rest = &block[end + CONTENT_END.len()..];
        }
    }
    found.into_iter().map(|(_, content)| content).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILE),
            format!("---\n{frontmatter}\n---\n\n{body}\n"),
        )
        .unwrap();
        dir
    }

    fn dirs(home: &Path) -> Dirs {
        Dirs {
            home: home.to_path_buf(),
            config_home: home.join(".config"),
            data_home: home.join(".local/share"),
            state_home: home.join(".local/state"),
            runtime_dir: None,
        }
    }

    struct Env {
        _dir: tempfile::TempDir,
        home: PathBuf,
        workspace: PathBuf,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        Env {
            _dir: dir,
            home,
            workspace,
        }
    }

    fn sources(env: &Env, config: SkillsConfig) -> SkillSources {
        SkillSources::new(&config, &dirs(&env.home))
    }

    #[test]
    fn finds_skills_and_the_workspace_wins() {
        let env = env();
        write_skill(
            &env.home.join(".agents/skills"),
            "review",
            "name: review\ndescription: User review.",
            "user",
        );
        write_skill(
            &env.home.join(".config/loom/skills"),
            "pdf",
            "name: pdf\ndescription: >\n  Work with\n  PDF files.",
            "pdf body",
        );
        write_skill(
            &env.workspace.join(".agents/skills"),
            "review",
            "name: review\ndescription: Project review.",
            "project",
        );
        // Not a skill: no SKILL.md.
        std::fs::create_dir_all(env.workspace.join(".loom/skills/empty")).unwrap();
        let catalog = sources(&env, SkillsConfig::default()).discover(&env.workspace);
        let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["pdf", "review"]);
        assert_eq!(
            catalog.get("review").unwrap().description,
            "Project review."
        );
        assert!(catalog.warnings.is_empty(), "{:?}", catalog.warnings);
        let section = catalog.prompt_section().unwrap();
        assert!(section.starts_with("# Skills"), "{section}");
        assert!(
            section.contains("\n- pdf: Work with PDF files."),
            "{section}"
        );
        assert!(section.contains("\n- review: Project review."), "{section}");
    }

    #[test]
    fn config_paths_disabled_names_and_switch() {
        let env = env();
        let extra = env.workspace.join("tools/one-skill");
        std::fs::create_dir_all(&extra).unwrap();
        write_skill(&extra, ".", "name: single\ndescription: One skill.", "body");
        write_skill(
            &env.workspace.join(".loom/skills"),
            "hidden",
            "description: Hidden.",
            "body",
        );
        let config = SkillsConfig {
            enabled: true,
            paths: vec![PathBuf::from("tools/one-skill")],
            disabled: vec!["hidden".into()],
        };
        let catalog = sources(&env, config.clone()).discover(&env.workspace);
        let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["single"]);

        let off = SkillsConfig {
            enabled: false,
            ..config
        };
        let catalog = sources(&env, off).discover(&env.workspace);
        assert!(catalog.skills.is_empty());
        assert_eq!(catalog.prompt_section(), None);
    }

    #[test]
    fn lenient_frontmatter_and_skipped_skills() {
        let env = env();
        let root = env.workspace.join(".agents/skills");
        // A colon in a plain value is not valid YAML; other agents accept it.
        write_skill(
            &root,
            "colon",
            "name: colon\ndescription: Use this when: the user asks",
            "body",
        );
        // Without a name, the directory names the skill.
        write_skill(&root, "unnamed", "description: No name.", "body");
        write_skill(&root, "no-description", "name: no-description", "body");
        std::fs::create_dir_all(root.join("no-frontmatter")).unwrap();
        std::fs::write(root.join("no-frontmatter").join(SKILL_FILE), "# Title\n").unwrap();
        let catalog = sources(&env, SkillsConfig::default()).discover(&env.workspace);
        let names: Vec<&str> = catalog.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["colon", "unnamed"]);
        assert_eq!(
            catalog.get("colon").unwrap().description,
            "Use this when: the user asks"
        );
        assert_eq!(catalog.warnings.len(), 2, "{:?}", catalog.warnings);
        assert!(catalog.warnings[0].contains("no description"));
        assert!(catalog.warnings[1].contains("frontmatter"));
    }

    #[tokio::test]
    async fn the_tool_loads_the_instructions() {
        let env = env();
        let dir = write_skill(
            &env.home.join(".agents/skills"),
            "pdf",
            "name: pdf\ndescription: PDF files.",
            "Run `scripts/extract.py`.",
        );
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/extract.py"), "print('hi')\n").unwrap();
        std::fs::write(dir.join("REFERENCE.md"), "# Reference\n").unwrap();
        // Files that tools made are not listed.
        for generated in [
            "scripts/__pycache__/extract.cpython-313.pyc",
            "scripts/extract.pyc",
            "node_modules/left-pad/index.js",
            "venv/pyvenv.cfg",
            "venv/lib/site.py",
            ".venv/lib/site.py",
            ".DS_Store",
            "Thumbs.db",
            "REFERENCE.md~",
        ] {
            std::fs::create_dir_all(dir.join(generated).parent().unwrap()).unwrap();
            std::fs::write(dir.join(generated), "").unwrap();
        }
        let tool = SkillTool::new(Arc::new(sources(&env, SkillsConfig::default())), 1024);
        let workspace = Arc::new(loom_tools::Workspace::new(&env.workspace).unwrap());
        let ctx = ToolContext::new(workspace);

        let out = tool.call(&ctx, json!({"name": "pdf"})).await.unwrap();
        assert!(
            out.content
                .starts_with("<skill_content name=\"pdf\">\nRun `scripts/extract.py`.\n"),
            "{}",
            out.content
        );
        assert!(!out.content.contains("description:"), "{}", out.content);
        let canonical = dir.canonicalize().unwrap();
        assert!(
            out.content
                .contains(&format!("Skill directory: {}", canonical.display()))
        );
        assert!(
            out.content
                .contains("<skill_files>\nREFERENCE.md\nscripts/extract.py\n</skill_files>"),
            "{}",
            out.content
        );
        assert!(out.content.ends_with(CONTENT_END));

        let err = tool
            .call(&ctx, json!({"name": "nope"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("the skills are pdf"), "{err}");
    }

    #[test]
    fn loaded_instructions_keep_the_last_of_each_skill() {
        let texts = [
            "<skill_content name=\"a\">\nold a\n</skill_content>",
            "Summary\n\n<skill_content name=\"b\">\nb\n</skill_content>",
            "<skill_content name=\"a\">\nnew a\n</skill_content>",
            "<skill_content name=\"c\">\ncut off",
        ];
        assert_eq!(
            loaded_instructions(texts.into_iter()),
            [
                "<skill_content name=\"b\">\nb\n</skill_content>",
                "<skill_content name=\"a\">\nnew a\n</skill_content>",
            ]
        );
    }
}
