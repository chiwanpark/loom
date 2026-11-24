use std::path::Path;

use loom_config::{Dirs, strip_managed_sections};
use loom_oracle_proto::{PresetFile, SKILLS_DIR};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedPreset {
    pub yaml: String,
    pub agents_md: String,
    pub files: Vec<PresetFile>,
    pub removed: Vec<&'static str>,
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

pub fn read_skills(root: &Path) -> Result<Vec<PresetFile>, String> {
    fn walk(dir: &Path, prefix: &str, files: &mut Vec<PresetFile>) -> Result<(), String> {
        let read_error = |err: std::io::Error| format!("cannot read {}: {err}", dir.display());
        let mut entries = std::fs::read_dir(dir)
            .map_err(read_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(read_error)?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let relative = format!("{prefix}/{name}");
            if path.is_dir() {
                walk(&path, &relative, files)?;
            } else {
                let content = std::fs::read_to_string(&path)
                    .map_err(|err| format!("cannot read {} as text: {err}", path.display()))?;
                files.push(PresetFile {
                    path: relative,
                    content,
                });
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, SKILLS_DIR, &mut files)?;
    Ok(files)
}

pub fn import_local_preset(dirs: &Dirs) -> Result<ImportedPreset, String> {
    let yaml = read_optional(&dirs.default_agent_config())?.unwrap_or_default();
    let (yaml, removed) = strip_managed_sections(&yaml).map_err(|err| err.to_string())?;
    let agents_md = read_optional(&dirs.system_agents_md())?.unwrap_or_default();
    let skills = dirs.config_dir().join(SKILLS_DIR);
    let files = if skills.is_dir() {
        read_skills(&skills)?
    } else {
        Vec::new()
    };
    Ok(ImportedPreset {
        yaml,
        agents_md,
        files,
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_configuration_becomes_a_preset() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs {
            home: dir.path().to_path_buf(),
            config_home: dir.path().join("config"),
            data_home: dir.path().join("data"),
            state_home: dir.path().join("state"),
            runtime_dir: None,
        };
        let empty = import_local_preset(&dirs).unwrap();
        assert_eq!((empty.yaml.as_str(), empty.files.len()), ("", 0));
        let config = dirs.config_dir();
        std::fs::create_dir_all(config.join("skills/review/.git")).unwrap();
        std::fs::write(
            config.join("agent.yaml"),
            "socket: /tmp/x.sock\nllm:\n  default_model: a/b\n",
        )
        .unwrap();
        std::fs::write(config.join("AGENTS.md"), "Be brief.\n").unwrap();
        std::fs::write(config.join("skills/review/SKILL.md"), "# Review\n").unwrap();
        std::fs::write(config.join("skills/review/.git/HEAD"), "x").unwrap();
        let imported = import_local_preset(&dirs).unwrap();
        assert_eq!(imported.removed, ["socket"]);
        assert!(!imported.yaml.contains("socket"), "{}", imported.yaml);
        assert_eq!(imported.agents_md, "Be brief.\n");
        assert_eq!(
            imported.files,
            [PresetFile {
                path: "skills/review/SKILL.md".into(),
                content: "# Review\n".into(),
            }]
        );
    }
}
