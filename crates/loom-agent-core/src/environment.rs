//! The environment of the commands that the agent runs: shell tool calls and
//! terminals. It comes from the agent's own environment, the `environment`
//! section of the configuration, and the workspace's env file, which is read
//! again when it changes.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use loom_config::{EnvironmentConfig, InheritEnvironment};
use loom_tools::env::{CommandEnv, MIN_SECRET_LEN, Redactor, parse_dotenv};

/// Names of the agent's variables that commands do not get by default, and
/// whose values the agent hides from the model.
pub const DEFAULT_EXCLUDES: &[&str] = &["*KEY*", "*SECRET*", "*TOKEN*", "*PASSWORD*"];

/// The variables that `inherit: core` keeps.
const CORE: &[&str] = &[
    "HOME", "LOGNAME", "PATH", "SHELL", "USER", "USERNAME", "TMPDIR", "TEMP", "TMP", "LANG",
    "LC_*", "TZ",
];

/// How often the agent checks again whether git ignores the env file, while
/// it does not.
const IGNORE_RECHECK: Duration = Duration::from_secs(10);

/// The environment at one moment, for one tool call or terminal.
#[derive(Debug, Default)]
pub struct EnvSnapshot {
    pub command: Arc<CommandEnv>,
    /// The names of the variables from the env file and `set`, sorted, each
    /// with whether the model may see its value.
    pub names: Vec<(String, bool)>,
    /// Problems for the user to fix, such as an env file that git does not
    /// ignore.
    pub warnings: Vec<String>,
}

/// Builds the environment of commands, and keeps the last one while the
/// configuration and the env file stay the same.
pub struct Environment {
    root: PathBuf,
    agent: Vec<(OsString, OsString)>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    file: FileCache,
    built: Option<Built>,
}

struct Built {
    config: EnvironmentConfig,
    stamp: Option<Stamp>,
    snapshot: Arc<EnvSnapshot>,
    /// When git did not ignore the env file, as of this time.
    not_ignored: Option<Instant>,
}

/// The values of the env file, the last ones that could be read.
#[derive(Default)]
struct FileCache {
    path: Option<PathBuf>,
    read: bool,
    stamp: Option<Stamp>,
    vars: Vec<(String, String)>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
        inode: metadata.ino(),
    })
}

impl Environment {
    /// The environment of commands in the workspace at `root`, from the
    /// agent's own environment.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_vars(root, std::env::vars_os().collect())
    }

    /// Like [`Environment::new`], with `vars` in place of the agent's own
    /// environment.
    pub fn with_vars(root: impl Into<PathBuf>, vars: Vec<(OsString, OsString)>) -> Self {
        Self {
            root: root.into(),
            agent: vars,
            state: Mutex::default(),
        }
    }

    /// The environment that `config` makes now.
    pub fn snapshot(&self, config: &EnvironmentConfig) -> Arc<EnvSnapshot> {
        let path = config.env_file.as_ref().map(|file| self.root.join(file));
        let stamp = path.as_deref().and_then(stamp_of);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(built) = &state.built
            && built.config == *config
            && built.stamp == stamp
            && built
                .not_ignored
                .is_none_or(|checked| checked.elapsed() < IGNORE_RECHECK)
        {
            return built.snapshot.clone();
        }
        if state.file.path != path {
            state.file = FileCache {
                path: path.clone(),
                ..FileCache::default()
            };
        }
        if !state.file.read || state.file.stamp != stamp {
            self.read_file(&mut state.file, stamp.clone());
        }
        let (snapshot, not_ignored) = self.build(config, &state.file, stamp.is_some());
        let snapshot = Arc::new(snapshot);
        state.built = Some(Built {
            config: config.clone(),
            stamp,
            snapshot: snapshot.clone(),
            not_ignored: not_ignored.then(Instant::now),
        });
        snapshot
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    /// Reads the env file again. When it cannot be read, its values stay as
    /// they were.
    fn read_file(&self, file: &mut FileCache, stamp: Option<Stamp>) {
        file.read = true;
        file.stamp = stamp;
        file.error = None;
        let Some(path) = file.path.clone() else {
            file.vars.clear();
            return;
        };
        if file.stamp.is_none() {
            file.vars.clear();
            return;
        }
        let parsed = std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| parse_dotenv(&text));
        match parsed {
            Ok(vars) => file.vars = vars,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "cannot read the env file");
                file.error = Some(format!(
                    "cannot read the env file {}: {err}; commands keep the values it had before",
                    self.display(&path)
                ));
            }
        }
    }

    /// The environment, and whether git does not ignore the env file.
    fn build(
        &self,
        config: &EnvironmentConfig,
        file: &FileCache,
        exists: bool,
    ) -> (EnvSnapshot, bool) {
        let vars = build_vars(&self.agent, config, &file.vars);
        let enabled = config.redaction.enabled;
        let allow: HashSet<&str> = config.redaction.allow.iter().map(String::as_str).collect();
        let mut warnings: Vec<String> = file.error.iter().cloned().collect();
        let mut names = BTreeMap::new();
        let mut secrets: Vec<(String, String)> = Vec::new();
        let mut short = Vec::new();
        for (name, value) in file.vars.iter().map(|(n, v)| (n, v)).chain(&config.set) {
            let hidden = enabled && !allow.contains(name.as_str());
            names.insert(name.clone(), hidden);
            if hidden {
                if !value.is_empty() && value.len() < MIN_SECRET_LEN && !short.contains(name) {
                    short.push(name.clone());
                }
                secrets.push((name.clone(), value.clone()));
            }
        }
        for name in short {
            warnings.push(format!(
                "the value of {name} is shorter than {MIN_SECRET_LEN} characters, so the agent \
                 cannot hide it from the model; add {name} to environment.redaction.allow if it \
                 is not secret"
            ));
        }
        if enabled {
            for (name, value) in &self.agent {
                let name = name.to_string_lossy();
                if any_match(DEFAULT_EXCLUDES, &name) && !allow.contains(name.as_ref()) {
                    secrets.push((name.into_owned(), value.to_string_lossy().into_owned()));
                }
            }
        }
        let mut protected = Vec::new();
        let mut not_ignored = false;
        if let Some(path) = file.path.as_ref().filter(|_| enabled) {
            protected.push(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()));
            if exists && !git_ignores(&self.root, path) {
                not_ignored = true;
                warnings.push(format!(
                    "git does not ignore the env file {}, so its secrets can be committed; \
                     add it to .gitignore",
                    self.display(path)
                ));
            }
        }
        let redactor = if enabled {
            Redactor::new(secrets.iter().map(|(n, v)| (n.as_str(), v.as_str())))
        } else {
            Redactor::default()
        };
        let snapshot = EnvSnapshot {
            command: Arc::new(CommandEnv {
                vars,
                redactor,
                protected,
                shown: allow.iter().map(|name| (*name).to_owned()).collect(),
            }),
            names: names.into_iter().collect(),
            warnings,
        };
        (snapshot, not_ignored)
    }
}

/// Whether git ignores `path`. A file outside a repository counts as ignored.
fn git_ignores(root: &Path, path: &Path) -> bool {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    !matches!(status.map(|s| s.code()), Ok(Some(1)))
}

/// The variables of a command, in this order: the agent's environment,
/// `inherit`, the default excludes, `exclude`, the env file, `set`, and
/// `include_only`.
pub fn build_vars(
    agent: &[(OsString, OsString)],
    config: &EnvironmentConfig,
    file: &[(String, String)],
) -> Vec<(OsString, OsString)> {
    let name = |n: &OsStr| n.to_string_lossy().into_owned();
    let mut vars: Vec<(OsString, OsString)> = agent
        .iter()
        .filter(|(n, _)| {
            let n = name(n);
            let inherited = match config.inherit {
                InheritEnvironment::All => true,
                InheritEnvironment::Core => any_match(CORE, &n),
                InheritEnvironment::None => false,
            };
            inherited
                && (config.ignore_default_excludes || !any_match(DEFAULT_EXCLUDES, &n))
                && !any_match(&config.exclude, &n)
        })
        .cloned()
        .collect();
    for (key, value) in file.iter().map(|(n, v)| (n, v)).chain(&config.set) {
        let key = OsString::from(key);
        match vars.iter_mut().find(|(n, _)| *n == key) {
            Some((_, v)) => *v = value.into(),
            None => vars.push((key, value.into())),
        }
    }
    if !config.include_only.is_empty() {
        vars.retain(|(n, _)| any_match(&config.include_only, &name(n)));
    }
    vars
}

fn any_match(patterns: &[impl AsRef<str>], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p.as_ref(), name))
}

/// Whether `name` matches `pattern`, where `*` stands for any text and `?`
/// for one character, ignoring case.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.to_uppercase().chars().collect();
    let name: Vec<char> = name.to_uppercase().chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(n, v)| (OsString::from(n), OsString::from(v)))
            .collect()
    }

    fn names(vars: &[(OsString, OsString)]) -> Vec<String> {
        vars.iter()
            .map(|(n, v)| format!("{}={}", n.to_string_lossy(), v.to_string_lossy()))
            .collect()
    }

    #[test]
    fn globs_ignore_case() {
        assert!(glob_match("*KEY*", "aws_access_key_id"));
        assert!(glob_match("AWS_*", "AWS_REGION"));
        assert!(glob_match("A?C", "abc"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("AWS_*", "MY_AWS_X"));
        assert!(!glob_match("A?C", "ac"));
        assert!(glob_match("*_TOKEN", "GH_TOKEN_TOKEN"));
    }

    #[test]
    fn variables_are_built_in_order() {
        let agent = vars(&[
            ("PATH", "/bin"),
            ("HOME", "/home/u"),
            ("GITHUB_TOKEN", "gh"),
            ("EDITOR", "vi"),
            ("AWS_PROFILE", "dev"),
        ]);
        let file = vec![
            ("AWS_PROFILE".to_owned(), "prod".to_owned()),
            ("DB_URL".to_owned(), "postgres://x".to_owned()),
        ];
        let mut config = EnvironmentConfig::default();
        assert_eq!(
            names(&build_vars(&agent, &config, &file)),
            [
                "PATH=/bin",
                "HOME=/home/u",
                "EDITOR=vi",
                "AWS_PROFILE=prod",
                "DB_URL=postgres://x"
            ]
        );
        config.inherit = InheritEnvironment::Core;
        config.ignore_default_excludes = true;
        config.set.insert("DB_URL".into(), "sqlite://y".into());
        assert_eq!(
            names(&build_vars(&agent, &config, &file)),
            [
                "PATH=/bin",
                "HOME=/home/u",
                "AWS_PROFILE=prod",
                "DB_URL=sqlite://y"
            ]
        );
        config.inherit = InheritEnvironment::All;
        config.exclude = vec!["editor".into()];
        config.include_only = vec!["PATH".into(), "*TOKEN*".into(), "DB_*".into()];
        assert_eq!(
            names(&build_vars(&agent, &config, &file)),
            ["PATH=/bin", "GITHUB_TOKEN=gh", "DB_URL=sqlite://y"]
        );
        config.inherit = InheritEnvironment::None;
        config.include_only.clear();
        assert_eq!(
            names(&build_vars(&agent, &config, &file)),
            ["AWS_PROFILE=prod", "DB_URL=sqlite://y"]
        );
    }

    #[test]
    fn the_env_file_is_read_again_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let env = Environment::with_vars(
            root,
            vars(&[("PATH", "/bin"), ("OPENAI_API_KEY", "sk-agent-secret-1")]),
        );
        let mut config = EnvironmentConfig::default();
        config.redaction.allow = vec!["REGION".into()];
        let first = env.snapshot(&config);
        assert!(first.names.is_empty());
        assert_eq!(names(&first.command.vars), ["PATH=/bin"]);
        // The agent's own secrets are hidden though commands do not get them.
        assert_eq!(
            first.command.redactor.redact("sk-agent-secret-1"),
            "[REDACTED:OPENAI_API_KEY]"
        );
        assert!(Arc::ptr_eq(&first, &env.snapshot(&config)));

        std::fs::write(
            root.join(".env"),
            "TOKEN=file-secret-1\nREGION=eu-west-1\nDEBUG=1\n",
        )
        .unwrap();
        let second = env.snapshot(&config);
        assert_eq!(
            second.names,
            [
                ("DEBUG".to_owned(), true),
                ("REGION".to_owned(), false),
                ("TOKEN".to_owned(), true)
            ]
        );
        assert!(
            names(&second.command.vars).contains(&"TOKEN=file-secret-1".to_owned()),
            "{:?}",
            second.command.vars
        );
        let redactor = &second.command.redactor;
        assert_eq!(redactor.redact("file-secret-1"), "[REDACTED:TOKEN]");
        assert_eq!(redactor.redact("eu-west-1"), "eu-west-1");
        assert!(second.command.is_protected(&root.join(".env")));
        assert!(second.command.shown.contains("REGION"));
        assert!(
            second.warnings.iter().any(|w| w.contains("DEBUG")),
            "{:?}",
            second.warnings
        );

        // An invalid file keeps the values it had, with a warning.
        std::fs::write(root.join(".env"), "TOKEN=file-secret-2\nBAD LINE\n").unwrap();
        let third = env.snapshot(&config);
        assert!(names(&third.command.vars).contains(&"TOKEN=file-secret-1".to_owned()));
        assert!(
            third.warnings[0].contains("line 2 cannot be parsed"),
            "{:?}",
            third.warnings
        );
        assert!(!third.warnings[0].contains("BAD"));

        std::fs::remove_file(root.join(".env")).unwrap();
        assert!(env.snapshot(&config).names.is_empty());

        // Without redaction, nothing is hidden or protected.
        config.redaction.enabled = false;
        std::fs::write(root.join(".env"), "TOKEN=file-secret-1\n").unwrap();
        let off = env.snapshot(&config);
        assert!(off.command.redactor.is_empty());
        assert!(off.command.protected.is_empty());
        assert_eq!(off.names, [("TOKEN".to_owned(), false)]);
    }

    #[test]
    fn an_env_file_that_git_does_not_ignore_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join(".env"), "TOKEN=file-secret-1\n").unwrap();
        let env = Environment::with_vars(root, Vec::new());
        let config = EnvironmentConfig::default();
        let warned = env.snapshot(&config);
        assert!(
            warned.warnings.iter().any(|w| w.contains(".gitignore")),
            "{:?}",
            warned.warnings
        );
        std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
        // The file did not change, but the configuration did.
        let mut config = config;
        config.set.insert("X".into(), "y".into());
        assert!(
            !env.snapshot(&config)
                .warnings
                .iter()
                .any(|w| w.contains(".gitignore"))
        );
    }
}
