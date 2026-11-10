use std::path::{Path, PathBuf};

use loom_config::{AgentConfig, Dirs};

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace {path}: {source}")]
    Missing {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("workspace {0} is not a directory")]
    NotDirectory(PathBuf),
    #[error("workspace {0} is outside allowed_roots in agent.yaml")]
    NotAllowed(PathBuf),
    #[error("no workspace given and lifecycle.default_workspace is not set")]
    NoDefault,
}

pub fn canonical_workspace(path: &Path, dirs: &Dirs) -> Result<PathBuf, WorkspaceError> {
    let expanded = dirs.expand(path);
    let canonical = std::fs::canonicalize(&expanded).map_err(|source| WorkspaceError::Missing {
        path: expanded.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(WorkspaceError::NotDirectory(canonical));
    }
    Ok(canonical)
}

pub fn resolve_workspace(
    path: Option<&Path>,
    config: &AgentConfig,
    dirs: &Dirs,
) -> Result<PathBuf, WorkspaceError> {
    let path = match path {
        Some(path) => path,
        None => config
            .lifecycle
            .default_workspace
            .as_deref()
            .ok_or(WorkspaceError::NoDefault)?,
    };
    let workspace = canonical_workspace(path, dirs)?;
    let allowed = config.allowed_roots.iter().any(|root| {
        std::fs::canonicalize(dirs.expand(root)).is_ok_and(|root| workspace.starts_with(root))
    });
    if !allowed {
        return Err(WorkspaceError::NotAllowed(workspace));
    }
    Ok(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(home: &Path) -> Dirs {
        Dirs {
            home: home.to_path_buf(),
            config_home: home.join(".config"),
            data_home: home.join(".local/share"),
            state_home: home.join(".local/state"),
            runtime_dir: None,
        }
    }

    #[test]
    fn checks_allowed_roots_and_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join("src/a")).unwrap();
        std::fs::create_dir_all(home.join("other")).unwrap();
        std::fs::write(home.join("file"), "").unwrap();
        let dirs = dirs(&home);
        let mut config = AgentConfig {
            allowed_roots: vec![home.join("src")],
            ..AgentConfig::default()
        };

        assert_eq!(
            resolve_workspace(Some(Path::new("~/src/a/../a")), &config, &dirs).unwrap(),
            home.join("src/a")
        );
        assert!(matches!(
            resolve_workspace(Some(Path::new("~/other")), &config, &dirs),
            Err(WorkspaceError::NotAllowed(_))
        ));
        assert!(matches!(
            resolve_workspace(Some(Path::new("~/missing")), &config, &dirs),
            Err(WorkspaceError::Missing { .. })
        ));
        config.allowed_roots = vec![home.clone()];
        assert!(matches!(
            resolve_workspace(Some(Path::new("~/file")), &config, &dirs),
            Err(WorkspaceError::NotDirectory(_))
        ));
        assert!(matches!(
            resolve_workspace(None, &config, &dirs),
            Err(WorkspaceError::NoDefault)
        ));
        config.lifecycle.default_workspace = Some(home.join("other"));
        assert_eq!(
            resolve_workspace(None, &config, &dirs).unwrap(),
            home.join("other")
        );
    }
}
