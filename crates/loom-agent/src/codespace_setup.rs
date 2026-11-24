use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use loom_config::valid_variable_name;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LOOM_SETUP: &str = ".loom/setup.sh";
pub const DEVCONTAINER_PATHS: &[&str] = &[".devcontainer/devcontainer.json", ".devcontainer.json"];
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const SETUP_DIR: &str = "setup";
const STATE_FILE: &str = "state.json";
const MAX_FILE: u64 = 1 << 20;
const USED_KEYS: &[&str] = &[
    "name",
    "containerEnv",
    "remoteEnv",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "$schema",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleCommand {
    Shell(String),
    Exec(Vec<String>),
    Parallel(Vec<(String, LifecycleCommand)>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevContainer {
    pub env: BTreeMap<String, String>,
    pub on_create: Option<LifecycleCommand>,
    pub update_content: Option<LifecycleCommand>,
    pub post_create: Option<LifecycleCommand>,
    pub post_start: Option<LifecycleCommand>,
    pub ignored: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setup {
    Script(PathBuf),
    DevContainer(PathBuf, DevContainer),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupClone {
    pub directory: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SetupState {
    #[serde(default)]
    created: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupOutcome {
    pub variables: BTreeMap<String, String>,
    pub failures: Vec<String>,
}

fn starts_comment(chars: &[char], at: usize) -> bool {
    chars.get(at) == Some(&'/') && matches!(chars.get(at + 1), Some('/' | '*'))
}

fn skip_space_and_comments(chars: &[char], mut at: usize) -> usize {
    loop {
        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        if !starts_comment(chars, at) {
            return at;
        }
        if chars[at + 1] == '/' {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else {
            at += 2;
            while at < chars.len() && !(chars[at] == '*' && chars.get(at + 1) == Some(&'/')) {
                at += 1;
            }
            at = (at + 2).min(chars.len());
        }
    }
}

pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            out.push(c);
            i += 1;
            while i < chars.len() {
                let d = chars[i];
                out.push(d);
                i += 1;
                if d == '\\' && i < chars.len() {
                    out.push(chars[i]);
                    i += 1;
                } else if d == '"' {
                    break;
                }
            }
        } else if starts_comment(&chars, i) {
            i = skip_space_and_comments(&chars, i);
            out.push(' ');
        } else if c == ',' {
            let next = skip_space_and_comments(&chars, i + 1);
            if !matches!(chars.get(next), Some('}' | ']')) {
                out.push(c);
            }
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn command(value: &Value, key: &str) -> anyhow::Result<Option<LifecycleCommand>> {
    let single = |value: &Value| -> anyhow::Result<LifecycleCommand> {
        match value {
            Value::String(text) => Ok(LifecycleCommand::Shell(text.clone())),
            Value::Array(items) => {
                let args: Option<Vec<String>> = items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect();
                match args {
                    Some(args) if !args.is_empty() => Ok(LifecycleCommand::Exec(args)),
                    _ => bail!("{key} has an array that is not a list of strings"),
                }
            }
            _ => bail!("{key} is not a string or an array of strings"),
        }
    };
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => {
            let mut parallel = Vec::new();
            for (name, value) in map {
                parallel.push((name.clone(), single(value)?));
            }
            Ok((!parallel.is_empty()).then_some(LifecycleCommand::Parallel(parallel)))
        }
        Some(value) => single(value).map(Some),
    }
}

pub fn parse_devcontainer(text: &str) -> anyhow::Result<DevContainer> {
    let value: Value =
        serde_json::from_str(&strip_jsonc(text)).context("the file is not valid JSON")?;
    let Value::Object(map) = &value else {
        bail!("the file is not a JSON object");
    };
    let mut env = BTreeMap::new();
    for key in ["containerEnv", "remoteEnv"] {
        let Some(found) = map.get(key) else {
            continue;
        };
        let Value::Object(vars) = found else {
            bail!("{key} is not an object");
        };
        for (name, value) in vars {
            if !valid_variable_name(name) {
                bail!("{key} has the variable {name:?}, which is not a valid name");
            }
            match value {
                Value::String(text) => {
                    env.insert(name.clone(), text.clone());
                }
                Value::Null => {
                    env.remove(name);
                }
                _ => bail!("{key}.{name} is not a string"),
            }
        }
    }
    let mut ignored: Vec<String> = map
        .keys()
        .filter(|key| !USED_KEYS.contains(&key.as_str()))
        .cloned()
        .collect();
    ignored.sort();
    Ok(DevContainer {
        env,
        on_create: command(&value, "onCreateCommand")?,
        update_content: command(&value, "updateContentCommand")?,
        post_create: command(&value, "postCreateCommand")?,
        post_start: command(&value, "postStartCommand")?,
        ignored,
    })
}

fn read_limited(path: &Path) -> anyhow::Result<String> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if metadata.len() > MAX_FILE {
        bail!("{} is larger than {MAX_FILE} bytes", path.display());
    }
    std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}

pub fn find_setup(clone: &Path) -> anyhow::Result<Option<Setup>> {
    let script = clone.join(LOOM_SETUP);
    if script.is_file() {
        return Ok(Some(Setup::Script(script)));
    }
    for relative in DEVCONTAINER_PATHS {
        let path = clone.join(relative);
        if path.is_file() {
            let parsed = parse_devcontainer(&read_limited(&path)?)
                .with_context(|| format!("cannot use {relative}"))?;
            return Ok(Some(Setup::DevContainer(path, parsed)));
        }
    }
    Ok(None)
}

pub struct Substitution<'a> {
    pub folder: &'a Path,
    pub id: &'a str,
    pub env: &'a BTreeMap<String, String>,
}

impl Substitution<'_> {
    fn variable(&self, name: &str) -> Option<String> {
        let basename = || {
            self.folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        match name {
            "containerWorkspaceFolder" | "localWorkspaceFolder" => {
                Some(self.folder.to_string_lossy().into_owned())
            }
            "containerWorkspaceFolderBasename" | "localWorkspaceFolderBasename" => Some(basename()),
            "devcontainerId" => Some(self.id.to_owned()),
            _ => {
                let (kind, rest) = name.split_once(':')?;
                if !matches!(kind, "containerEnv" | "localEnv" | "env") {
                    return None;
                }
                let (var, default) = match rest.split_once(':') {
                    Some((var, default)) => (var, Some(default)),
                    None => (rest, None),
                };
                Some(
                    self.env
                        .get(var)
                        .cloned()
                        .or_else(|| default.map(str::to_owned))
                        .unwrap_or_default(),
                )
            }
        }
    }

    pub fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after
                .find('}')
                .and_then(|end| Some((end, self.variable(&after[..end])?)))
            {
                Some((end, value)) => {
                    out.push_str(&value);
                    rest = &after[end + 1..];
                }
                None => {
                    out.push_str("${");
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    fn command(&self, command: &LifecycleCommand) -> LifecycleCommand {
        match command {
            LifecycleCommand::Shell(text) => LifecycleCommand::Shell(self.apply(text)),
            LifecycleCommand::Exec(args) => {
                LifecycleCommand::Exec(args.iter().map(|arg| self.apply(arg)).collect())
            }
            LifecycleCommand::Parallel(items) => LifecycleCommand::Parallel(
                items
                    .iter()
                    .map(|(name, item)| (name.clone(), self.command(item)))
                    .collect(),
            ),
        }
    }
}

pub struct Runner {
    pub log: PathBuf,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
}

impl Runner {
    fn log_line(&self, line: &str) {
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(err) = written {
            tracing::warn!(error = %err, path = %self.log.display(), "cannot write the setup log");
        }
    }

    async fn run_one(&self, cwd: &Path, argv: &[String]) -> anyhow::Result<()> {
        let Some((program, args)) = argv.split_first() else {
            bail!("the command is empty");
        };
        self.log_line(&format!("$ {}", argv.join(" ")));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("cannot open {}", self.log.display()))?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(&self.env)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run {program}"))?;
        let status = tokio::time::timeout(self.timeout, child.wait())
            .await
            .map_err(|_| anyhow::anyhow!("{program} took longer than {:?}", self.timeout))?
            .with_context(|| format!("cannot wait for {program}"))?;
        if !status.success() {
            bail!(
                "{program} ended with {}",
                status
                    .code()
                    .map_or("a signal".to_owned(), |code| format!("exit code {code}"))
            );
        }
        Ok(())
    }

    pub async fn run(&self, cwd: &Path, command: &LifecycleCommand) -> anyhow::Result<()> {
        match command {
            LifecycleCommand::Shell(text) => {
                self.run_one(cwd, &["/bin/sh".into(), "-c".into(), text.clone()])
                    .await
            }
            LifecycleCommand::Exec(args) => self.run_one(cwd, args).await,
            LifecycleCommand::Parallel(items) => {
                let runs = items.iter().map(|(name, item)| async move {
                    let argv = match item {
                        LifecycleCommand::Shell(text) => {
                            vec!["/bin/sh".into(), "-c".into(), text.clone()]
                        }
                        LifecycleCommand::Exec(args) => args.clone(),
                        LifecycleCommand::Parallel(_) => {
                            bail!("the command {name} has commands in it")
                        }
                    };
                    self.run_one(cwd, &argv)
                        .await
                        .with_context(|| format!("the command {name} failed"))
                });
                for result in futures_util::future::join_all(runs).await {
                    result?;
                }
                Ok(())
            }
        }
    }
}

fn load_state(path: &Path) -> SetupState {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &SetupState) {
    let result = serde_json::to_vec_pretty(state)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| {
            let partial = path.with_extension("partial");
            std::fs::write(&partial, bytes)?;
            std::fs::rename(&partial, path)?;
            Ok(())
        });
    if let Err(err) = result {
        tracing::warn!(error = %format!("{err:#}"), "cannot keep the setup state");
    }
}

#[async_trait::async_trait]
pub trait SetupProgress: Send {
    async fn step(&mut self, step: &str);
}

pub struct SetupContext<'a> {
    pub data_dir: &'a Path,
    pub codespace: &'a str,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
}

pub fn read_variables(clones: &[SetupClone], context: &SetupContext<'_>) -> SetupOutcome {
    let mut outcome = SetupOutcome::default();
    let mut env = context.env.clone();
    for clone in clones {
        match find_setup(&clone.path) {
            Ok(Some(Setup::DevContainer(_, devcontainer))) => {
                let values: Vec<(String, String)> = {
                    let substitution = Substitution {
                        folder: &clone.path,
                        id: context.codespace,
                        env: &env,
                    };
                    devcontainer
                        .env
                        .iter()
                        .map(|(name, value)| (name.clone(), substitution.apply(value)))
                        .collect()
                };
                for (name, value) in values {
                    outcome.variables.insert(name.clone(), value.clone());
                    env.insert(name, value);
                }
            }
            Ok(_) => {}
            Err(err) => outcome
                .failures
                .push(format!("setup of {}: {err:#}", clone.directory)),
        }
    }
    outcome
}

pub async fn run_setup(
    clones: &[SetupClone],
    context: SetupContext<'_>,
    progress: &mut dyn SetupProgress,
) -> anyhow::Result<SetupOutcome> {
    let dir = context.data_dir.join(SETUP_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot make {}", dir.display()))?;
    let state_path = dir.join(STATE_FILE);
    let mut state = load_state(&state_path);
    let mut outcome = read_variables(clones, &context);
    for failure in &outcome.failures {
        tracing::warn!(error = %failure, "cannot read the setup of a repository");
        progress.step(failure).await;
    }
    let mut env = context.env.clone();
    env.extend(outcome.variables.clone());
    for clone in clones {
        let Ok(Some(setup)) = find_setup(&clone.path) else {
            continue;
        };
        let runner = Runner {
            log: dir.join(format!("{}.log", clone.directory)),
            env: env.clone(),
            timeout: context.timeout,
        };
        let substitution = Substitution {
            folder: &clone.path,
            id: context.codespace,
            env: &env,
        };
        let created = state.created.contains_key(&clone.directory);
        let mut phases: Vec<(&str, LifecycleCommand)> = Vec::new();
        match &setup {
            Setup::Script(path) => {
                if !created {
                    phases.push((
                        LOOM_SETUP,
                        LifecycleCommand::Exec(vec![
                            "/bin/bash".into(),
                            path.to_string_lossy().into_owned(),
                        ]),
                    ));
                }
            }
            Setup::DevContainer(path, devcontainer) => {
                if !created {
                    if !devcontainer.ignored.is_empty() {
                        runner.log_line(&format!(
                            "# {}: Loom does not use {}",
                            path.display(),
                            devcontainer.ignored.join(", ")
                        ));
                    }
                    for (name, command) in [
                        ("onCreateCommand", &devcontainer.on_create),
                        ("updateContentCommand", &devcontainer.update_content),
                        ("postCreateCommand", &devcontainer.post_create),
                    ] {
                        if let Some(command) = command {
                            phases.push((name, substitution.command(command)));
                        }
                    }
                }
                if let Some(command) = &devcontainer.post_start {
                    phases.push(("postStartCommand", substitution.command(command)));
                }
            }
        }
        if phases.is_empty() {
            continue;
        }
        progress.step(&format!("setup: {}", clone.directory)).await;
        let mut error = None;
        for (name, command) in &phases {
            runner.log_line(&format!("# {name}"));
            if let Err(err) = runner.run(&clone.path, command).await {
                let message = format!(
                    "setup of {}: {name} failed: {err:#}; see {}",
                    clone.directory,
                    runner.log.display()
                );
                runner.log_line(&format!("# {message}"));
                tracing::warn!(error = %message, "a setup command failed");
                error = Some(message);
                break;
            }
        }
        if !created {
            state.created.insert(clone.directory.clone(), error.clone());
            save_state(&state_path, &state);
        }
        if let Some(message) = error {
            progress.step(&message).await;
            outcome.failures.push(message);
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonc(text: &str) -> String {
        text.replace("<line>", "//")
            .replace("<open>", "/*")
            .replace("<close>", "*/")
    }

    #[test]
    fn comments_and_trailing_commas_go_away() {
        let text = jsonc(
            r#"{
            <line> a note
            "name": "a <line> b", <open> block <close>
            "url": "http:<line>x/<open>y<close>",
            "list": [1, 2,],
            "escaped": "a \"quoted <line> text\"",
            "last": true, <line> end
            <open> more <close>
        }"#,
        );
        let value: Value = serde_json::from_str(&strip_jsonc(&text)).unwrap();
        assert_eq!(value["name"], jsonc("a <line> b"));
        assert_eq!(value["url"], jsonc("http:<line>x/<open>y<close>"));
        assert_eq!(value["list"], serde_json::json!([1, 2]));
        assert_eq!(value["escaped"], jsonc("a \"quoted <line> text\""));
        assert_eq!(value["last"], true);
    }

    #[test]
    fn the_subset_of_devcontainer_json_is_read() {
        let parsed = parse_devcontainer(
            r#"{
                "image": "mcr.microsoft.com/devcontainers/base",
                "features": {},
                "containerEnv": {"A": "1", "B": "container"},
                "remoteEnv": {"B": "remote", "A": null},
                "onCreateCommand": "make deps",
                "postCreateCommand": ["npm", "ci"],
                "postStartCommand": {"server": "npm start", "watch": ["npm", "run", "watch"]},
            }"#,
        )
        .unwrap();
        assert_eq!(
            parsed.env,
            BTreeMap::from([("B".to_owned(), "remote".to_owned())])
        );
        assert_eq!(
            parsed.on_create,
            Some(LifecycleCommand::Shell("make deps".into()))
        );
        assert_eq!(parsed.update_content, None);
        assert_eq!(
            parsed.post_create,
            Some(LifecycleCommand::Exec(vec!["npm".into(), "ci".into()]))
        );
        let Some(LifecycleCommand::Parallel(items)) = parsed.post_start else {
            panic!("expected parallel commands");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(parsed.ignored, vec!["features", "image"]);
        for bad in [
            "[]",
            r#"{"containerEnv": {"A B": "x"}}"#,
            r#"{"containerEnv": {"A": 1}}"#,
            r#"{"postCreateCommand": 3}"#,
            r#"{"postCreateCommand": []}"#,
        ] {
            assert!(parse_devcontainer(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn variables_are_replaced() {
        let env = BTreeMap::from([("HOME".to_owned(), "/home/dev".to_owned())]);
        let substitution = Substitution {
            folder: Path::new("/workspaces/app"),
            id: "c1",
            env: &env,
        };
        assert_eq!(
            substitution.apply(
                "${containerWorkspaceFolder}/bin:${localEnv:HOME}:${containerEnv:NONE:x}:${devcontainerId}:${containerWorkspaceFolderBasename}"
            ),
            "/workspaces/app/bin:/home/dev:x:c1:app"
        );
        assert_eq!(substitution.apply("${unknown} ${"), "${unknown} ${");
    }

    #[derive(Default)]
    struct Steps(Vec<String>);

    #[async_trait::async_trait]
    impl SetupProgress for Steps {
        async fn step(&mut self, step: &str) {
            self.0.push(step.to_owned());
        }
    }

    #[tokio::test]
    async fn setup_runs_once_and_post_start_runs_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let app = dir.path().join("app");
        let lib = dir.path().join("lib");
        let plain = dir.path().join("plain");
        for path in [&app, &lib, &plain] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::create_dir_all(app.join(".devcontainer")).unwrap();
        std::fs::write(
            app.join(".devcontainer/devcontainer.json"),
            jsonc(
                r#"{
                <line> the app
                "containerEnv": {"GREETING": "hello ${containerWorkspaceFolderBasename}"},
                "onCreateCommand": "echo \"$GREETING $SECRET\" >> created",
                "postCreateCommand": {"a": "touch a", "b": ["touch", "b"]},
                "postStartCommand": "echo start >> started",
            }"#,
            ),
        )
        .unwrap();
        std::fs::create_dir_all(lib.join(".loom")).unwrap();
        std::fs::write(
            lib.join(".loom/setup.sh"),
            "echo \"$GREETING\" >> ran\nexit 3\n",
        )
        .unwrap();
        std::fs::write(
            lib.join(".devcontainer.json"),
            "{\"containerEnv\":{\"X\":\"y\"}}",
        )
        .unwrap();
        let clones: Vec<SetupClone> = [("app", &app), ("lib", &lib), ("plain", &plain)]
            .into_iter()
            .map(|(directory, path)| SetupClone {
                directory: directory.into(),
                path: path.clone(),
            })
            .collect();
        let context = || SetupContext {
            data_dir: &data,
            codespace: "c1",
            env: BTreeMap::from([
                ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
                ("SECRET".to_owned(), "s3".to_owned()),
            ]),
            timeout: Duration::from_secs(30),
        };
        let mut steps = Steps::default();
        let outcome = run_setup(&clones, context(), &mut steps).await.unwrap();
        assert_eq!(
            outcome.variables,
            BTreeMap::from([("GREETING".to_owned(), "hello app".to_owned())])
        );
        assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
        assert!(outcome.failures[0].contains("exit code 3"), "{outcome:?}");
        assert_eq!(steps.0[0], "setup: app");
        assert_eq!(steps.0[1], "setup: lib");
        assert_eq!(
            std::fs::read_to_string(app.join("created")).unwrap(),
            "hello app s3\n"
        );
        assert!(app.join("a").exists() && app.join("b").exists());
        assert_eq!(
            std::fs::read_to_string(lib.join("ran")).unwrap(),
            "hello app\n"
        );
        let log = std::fs::read_to_string(data.join("setup/app.log")).unwrap();
        assert!(log.contains("# onCreateCommand"), "{log}");

        let mut steps = Steps::default();
        let outcome = run_setup(&clones, context(), &mut steps).await.unwrap();
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert_eq!(steps.0, vec!["setup: app"]);
        assert_eq!(
            std::fs::read_to_string(app.join("created")).unwrap(),
            "hello app s3\n"
        );
        assert_eq!(
            std::fs::read_to_string(app.join("started")).unwrap(),
            "start\nstart\n"
        );
        assert_eq!(
            std::fs::read_to_string(lib.join("ran")).unwrap(),
            "hello app\n"
        );
    }
}
