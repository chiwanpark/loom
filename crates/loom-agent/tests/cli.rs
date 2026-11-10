use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use loom_client::{AgentEntry, AgentTarget, Client, ClientError, Registry, connect_local};
use loom_transport::connect_command;

const BIN: &str = env!("CARGO_BIN_EXE_loom-agent");

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for ws in ["ws1", "ws2", "ws3"] {
            std::fs::create_dir_all(root.join("home/work").join(ws)).unwrap();
        }
        std::fs::create_dir_all(root.join("home/outside")).unwrap();
        Self { _dir: dir, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn ws(&self, name: &str) -> PathBuf {
        self.home().join("work").join(name)
    }

    fn runtime(&self) -> PathBuf {
        self.root.join("run")
    }

    fn registry(&self) -> Registry {
        Registry::new(self.runtime().join("loom"))
    }

    fn config_path(&self) -> PathBuf {
        self.home().join(".config/loom/agent.yaml")
    }

    fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.runtime())
            .env("LOOM_LOG", "info");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        self.apply(&mut command);
        command.args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn tokio_command(&self, args: &[String]) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(BIN);
        self.apply(command.as_std_mut());
        command.args(args);
        command
    }

    fn ps(&self) -> Vec<AgentEntry> {
        let output = self.run(&["ps", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    async fn wait_for_agents(&self, count: usize) -> Vec<AgentEntry> {
        for _ in 0..400 {
            let agents = self.ps();
            if agents.len() == count {
                return agents;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("expected {count} agents, found {:?}", self.ps());
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_stdio_spawn_and_stop() {
    let env = Env::new();

    let output = env.run(&["config", "path"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        path_arg(&env.config_path())
    );
    assert!(env.run(&["config", "init"]).status.success());
    assert!(!env.run(&["config", "init"]).status.success());
    assert!(env.run(&["config", "check"]).status.success());
    std::fs::write(
        env.config_path(),
        "allowed_roots:\n- ~/work\nlogging:\n  level: debug\n",
    )
    .unwrap();
    assert!(env.run(&["config", "check"]).status.success());
    let schema = env.run(&["config", "schema"]);
    assert!(String::from_utf8_lossy(&schema.stdout).contains("allowed_roots"));
    assert!(env.ps().is_empty());

    let mut server = env
        .command(&["serve", "--workspace", "~/work/ws1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let agents = env.wait_for_agents(1).await;
    assert_eq!(agents[0].workspace, env.ws("ws1"));
    assert_eq!(agents[0].pid, server.id());

    let duplicate = env.run(&["serve", "--workspace", &path_arg(&env.ws("ws1"))]);
    assert_eq!(duplicate.status.code(), Some(3), "{duplicate:?}");
    let outside = env.run(&["serve", "--workspace", "~/outside"]);
    assert!(stderr(&outside).contains("allowed_roots"), "{outside:?}");

    let target = AgentTarget::Workspace {
        path: env.ws("ws1"),
        spawn: false,
    };
    let local = connect_local(&env.registry(), &target, Path::new(BIN))
        .await
        .unwrap();
    let (local, _notes) = Client::connect(local, "test-local").await.unwrap();
    assert_eq!(local.agent().workspace, env.ws("ws1"));
    assert_eq!(local.agent().instance.0, agents[0].instance);
    assert!(!local.list_providers().await.unwrap().is_empty());

    let bridged = connect_command(env.tokio_command(&target.stdio_args())).unwrap();
    let (bridged, _notes) = Client::connect(bridged, "test-bridge").await.unwrap();
    let session = bridged.create_session(None).await.unwrap();
    let seen = local.list_sessions().await.unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].id, session.id);

    let missing = AgentTarget::Workspace {
        path: env.ws("ws3"),
        spawn: false,
    };
    let stream = connect_command(env.tokio_command(&missing.stdio_args())).unwrap();
    match Client::connect(stream, "test").await {
        Err(ClientError::Connect { diagnostics, .. }) => {
            assert!(diagnostics.contains("no agent is running"), "{diagnostics}")
        }
        Err(other) => panic!("unexpected error {other:?}"),
        Ok(_) => panic!("connected to a missing agent"),
    }
    let outside = AgentTarget::Workspace {
        path: env.home().join("outside"),
        spawn: true,
    };
    let stream = connect_command(env.tokio_command(&outside.stdio_args())).unwrap();
    match Client::connect(stream, "test").await {
        Err(ClientError::Connect { diagnostics, .. }) => {
            assert!(diagnostics.contains("allowed_roots"), "{diagnostics}")
        }
        Err(other) => panic!("unexpected error {other:?}"),
        Ok(_) => panic!("spawned outside allowed_roots"),
    }

    let spawn = AgentTarget::Workspace {
        path: env.ws("ws2"),
        spawn: true,
    };
    let spawned = connect_command(env.tokio_command(&spawn.stdio_args())).unwrap();
    let (spawned, _notes) = Client::connect(spawned, "test-spawn").await.unwrap();
    assert_eq!(spawned.agent().workspace, env.ws("ws2"));
    assert_eq!(env.wait_for_agents(2).await.len(), 2);
    assert!(
        env.home()
            .join(".local/state/loom/logs")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );

    let again = connect_command(env.tokio_command(&spawn.stdio_args())).unwrap();
    let (again, _notes) = Client::connect(again, "test-spawn-again").await.unwrap();
    assert_eq!(again.agent().instance, spawned.agent().instance);

    let ps = env.run(&["ps"]);
    let table = String::from_utf8_lossy(&ps.stdout);
    assert!(table.contains("INSTANCE") && table.contains(&path_arg(&env.ws("ws2"))));

    let stopped = env.run(&["stop", "--workspace", &path_arg(&env.ws("ws2"))]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert_eq!(env.wait_for_agents(1).await.len(), 1);
    let err = spawned.list_sessions().await.unwrap_err();
    assert!(matches!(err, ClientError::Closed), "{err:?}");

    let stopped = env.run(&["stop", "--all"]);
    assert!(stopped.status.success(), "{stopped:?}");
    let status = server.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(env.ps().is_empty());
    let leftovers: Vec<_> = env
        .runtime()
        .join("loom/agents")
        .read_dir()
        .unwrap()
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let none = env.run(&["stop", "--all"]);
    assert!(none.status.success() && none.stdout.is_empty(), "{none:?}");
    let unknown = env.run(&["stop", "--instance", "nope"]);
    assert!(
        stderr(&unknown).contains("no agent with instance nope"),
        "{unknown:?}"
    );
}
