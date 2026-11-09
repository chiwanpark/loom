use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;

pub const CONFIG_ENV: &str = "LOOM_CONFIG";
pub const APP_DIR: &str = "loom";
pub const AGENT_CONFIG_FILE: &str = "agent.yaml";
pub const AGENTS_MD: &str = "AGENTS.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
    pub runtime_dir: Option<PathBuf>,
}

impl Dirs {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, ConfigError> {
        let home = absolute_var(&get, "HOME").ok_or(ConfigError::NoHome)?;
        let xdg = |name: &str, fallback: &str| {
            absolute_var(&get, name).unwrap_or_else(|| home.join(fallback))
        };
        Ok(Self {
            config_home: xdg("XDG_CONFIG_HOME", ".config"),
            data_home: xdg("XDG_DATA_HOME", ".local/share"),
            state_home: xdg("XDG_STATE_HOME", ".local/state"),
            runtime_dir: absolute_var(&get, "XDG_RUNTIME_DIR"),
            home,
        })
    }

    pub fn config_dir(&self) -> PathBuf {
        self.config_home.join(APP_DIR)
    }

    pub fn default_agent_config(&self) -> PathBuf {
        self.config_dir().join(AGENT_CONFIG_FILE)
    }

    pub fn system_agents_md(&self) -> PathBuf {
        self.config_dir().join(AGENTS_MD)
    }

    pub fn default_data_dir(&self) -> PathBuf {
        self.data_home.join(APP_DIR)
    }

    pub fn default_runtime_dir(&self) -> PathBuf {
        match &self.runtime_dir {
            Some(dir) => dir.join(APP_DIR),
            None => self.state_home.join(APP_DIR).join("run"),
        }
    }

    pub fn agent_config_path(
        &self,
        explicit: Option<&Path>,
        env_value: Option<OsString>,
    ) -> PathBuf {
        if let Some(path) = explicit {
            return self.expand(path);
        }
        match env_value.filter(|value| !value.is_empty()) {
            Some(value) => self.expand(Path::new(&value)),
            None => self.default_agent_config(),
        }
    }

    pub fn expand(&self, path: &Path) -> PathBuf {
        expand_home(path, &self.home)
    }
}

pub fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

fn absolute_var(get: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<PathBuf> {
    get(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn dirs(vars: &[(&str, &str)]) -> Result<Dirs, ConfigError> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        Dirs::from_vars(|name| vars.get(name).cloned())
    }

    #[test]
    fn falls_back_to_home_based_defaults() {
        let dirs = dirs(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(
            dirs.default_agent_config(),
            Path::new("/home/u/.config/loom/agent.yaml")
        );
        assert_eq!(
            dirs.default_data_dir(),
            Path::new("/home/u/.local/share/loom")
        );
        assert_eq!(
            dirs.default_runtime_dir(),
            Path::new("/home/u/.local/state/loom/run")
        );
        assert_eq!(
            dirs.system_agents_md(),
            Path::new("/home/u/.config/loom/AGENTS.md")
        );
    }

    #[test]
    fn honors_xdg_variables_and_ignores_relative_ones() {
        let dirs = dirs(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_DATA_HOME", "relative"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ])
        .unwrap();
        assert_eq!(dirs.config_dir(), Path::new("/cfg/loom"));
        assert_eq!(
            dirs.default_data_dir(),
            Path::new("/home/u/.local/share/loom")
        );
        assert_eq!(dirs.default_runtime_dir(), Path::new("/run/user/1000/loom"));
    }

    #[test]
    fn requires_home() {
        assert!(matches!(dirs(&[]), Err(ConfigError::NoHome)));
    }

    #[test]
    fn resolves_config_path_by_priority() {
        let dirs = dirs(&[("HOME", "/home/u")]).unwrap();
        assert_eq!(
            dirs.agent_config_path(Some(Path::new("~/a.yaml")), Some("/env.yaml".into())),
            Path::new("/home/u/a.yaml")
        );
        assert_eq!(
            dirs.agent_config_path(None, Some("/env.yaml".into())),
            Path::new("/env.yaml")
        );
        assert_eq!(
            dirs.agent_config_path(None, Some("".into())),
            Path::new("/home/u/.config/loom/agent.yaml")
        );
    }

    #[test]
    fn expands_only_leading_tilde() {
        let home = Path::new("/home/u");
        assert_eq!(expand_home(Path::new("~"), home), Path::new("/home/u"));
        assert_eq!(
            expand_home(Path::new("~/src"), home),
            Path::new("/home/u/src")
        );
        assert_eq!(expand_home(Path::new("/a/~"), home), Path::new("/a/~"));
        assert_eq!(expand_home(Path::new("~other"), home), Path::new("~other"));
    }
}
