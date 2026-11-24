use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

pub const LOOM_SETUP: &str = ".loom/setup.sh";
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const SETUP_DIR: &str = "setup";
const STATE_FILE: &str = "state.json";

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
    pub failures: Vec<String>,
}

struct Runner {
    log: PathBuf,
    env: BTreeMap<String, String>,
    timeout: Duration,
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

    async fn run(&self, cwd: &Path, script: &Path) -> anyhow::Result<()> {
        self.log_line(&format!("$ bash {}", script.display()));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("cannot open {}", self.log.display()))?;
        let mut command = tokio::process::Command::new("/bin/bash");
        command
            .arg(script)
            .current_dir(cwd)
            .env_clear()
            .envs(&self.env)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        let mut child = command.spawn().context("cannot run bash")?;
        let status = tokio::time::timeout(self.timeout, child.wait())
            .await
            .map_err(|_| anyhow::anyhow!("the script took longer than {:?}", self.timeout))?
            .context("cannot wait for bash")?;
        if !status.success() {
            bail!(
                "the script ended with {}",
                status
                    .code()
                    .map_or("a signal".to_owned(), |code| format!("exit code {code}"))
            );
        }
        Ok(())
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
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
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
    let mut outcome = SetupOutcome::default();
    for clone in clones {
        let script = clone.path.join(LOOM_SETUP);
        if !script.is_file() || state.created.contains_key(&clone.directory) {
            continue;
        }
        let runner = Runner {
            log: dir.join(format!("{}.log", clone.directory)),
            env: context.env.clone(),
            timeout: context.timeout,
        };
        progress.step(&format!("setup: {}", clone.directory)).await;
        let error = match runner.run(&clone.path, &script).await {
            Ok(()) => None,
            Err(err) => {
                let message = format!(
                    "setup of {}: {LOOM_SETUP} failed: {err:#}; see {}",
                    clone.directory,
                    runner.log.display()
                );
                runner.log_line(&format!("# {message}"));
                tracing::warn!(error = %message, "a setup script failed");
                Some(message)
            }
        };
        state.created.insert(clone.directory.clone(), error.clone());
        save_state(&state_path, &state);
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

    #[derive(Default)]
    struct Steps(Vec<String>);

    #[async_trait::async_trait]
    impl SetupProgress for Steps {
        async fn step(&mut self, step: &str) {
            self.0.push(step.to_owned());
        }
    }

    #[tokio::test]
    async fn each_setup_script_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let app = dir.path().join("app");
        let lib = dir.path().join("lib");
        let plain = dir.path().join("plain");
        for path in [&app, &lib, &plain] {
            std::fs::create_dir_all(path.join(".loom")).unwrap();
        }
        std::fs::write(
            app.join(LOOM_SETUP),
            "echo \"$SECRET $(basename \"$PWD\")\" >> ran\n",
        )
        .unwrap();
        std::fs::write(lib.join(LOOM_SETUP), "echo lib >> ran\nexit 3\n").unwrap();
        let clones: Vec<SetupClone> = [("app", &app), ("lib", &lib), ("plain", &plain)]
            .into_iter()
            .map(|(directory, path)| SetupClone {
                directory: directory.into(),
                path: path.clone(),
            })
            .collect();
        let context = || SetupContext {
            data_dir: &data,
            env: BTreeMap::from([
                ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
                ("SECRET".to_owned(), "s3".to_owned()),
            ]),
            timeout: Duration::from_secs(30),
        };
        let mut steps = Steps::default();
        let outcome = run_setup(&clones, context(), &mut steps).await.unwrap();
        assert_eq!(outcome.failures.len(), 1, "{outcome:?}");
        assert!(outcome.failures[0].contains("exit code 3"), "{outcome:?}");
        assert_eq!(steps.0[..2], ["setup: app", "setup: lib"]);
        assert_eq!(
            std::fs::read_to_string(app.join("ran")).unwrap(),
            "s3 app\n"
        );
        let log = std::fs::read_to_string(data.join("setup/lib.log")).unwrap();
        assert!(log.contains("exit code 3"), "{log}");

        let mut steps = Steps::default();
        let outcome = run_setup(&clones, context(), &mut steps).await.unwrap();
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        assert!(steps.0.is_empty(), "{:?}", steps.0);
        assert_eq!(
            std::fs::read_to_string(app.join("ran")).unwrap(),
            "s3 app\n"
        );
        assert_eq!(std::fs::read_to_string(lib.join("ran")).unwrap(), "lib\n");
    }
}
