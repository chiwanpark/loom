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

    let kept = spawned.create_session(None).await.unwrap();
    let stopped = env.run(&["stop", "--workspace", &path_arg(&env.ws("ws2"))]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert_eq!(env.wait_for_agents(1).await.len(), 1);
    let err = spawned.list_sessions().await.unwrap_err();
    assert!(matches!(err, ClientError::Closed), "{err:?}");

    let respawned = connect_command(env.tokio_command(&spawn.stdio_args())).unwrap();
    let (respawned, _notes) = Client::connect(respawned, "test-respawn").await.unwrap();
    assert_ne!(respawned.agent().instance, spawned.agent().instance);
    let restored = respawned.list_sessions().await.unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, kept.id);
    let stopped = env.run(&["stop", "--workspace", &path_arg(&env.ws("ws2"))]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert_eq!(env.wait_for_agents(1).await.len(), 1);

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

#[tokio::test(flavor = "multi_thread")]
async fn log_level_follows_config_reload() {
    let env = Env::new();
    std::fs::create_dir_all(env.config_path().parent().unwrap()).unwrap();
    std::fs::write(
        env.config_path(),
        "allowed_roots:\n- ~/work\nlogging:\n  level: warn\n",
    )
    .unwrap();
    let log_path = env.root.join("serve.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut server = env
        .command(&["serve", "--workspace", "~/work/ws1"])
        .env_remove("LOOM_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    env.wait_for_agents(1).await;
    let read_log = || std::fs::read_to_string(&log_path).unwrap();
    assert!(!read_log().contains("agent ready"), "{}", read_log());

    std::fs::write(
        env.config_path(),
        "allowed_roots:\n- ~/work\nlogging:\n  level: debug\n",
    )
    .unwrap();
    let mut changed = false;
    for _ in 0..200 {
        if read_log().contains("log level changed") {
            changed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stopped = env.run(&["stop", "--all"]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(server.wait().unwrap().success());
    assert!(changed, "{}", read_log());
    assert!(read_log().contains("agent stopped"), "{}", read_log());
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_manage_siblings_over_the_protocol() {
    let env = Env::new();
    std::fs::create_dir_all(env.config_path().parent().unwrap()).unwrap();
    std::fs::write(env.config_path(), "allowed_roots:\n- ~/work\n").unwrap();
    let mut server = env
        .command(&["serve", "--workspace", "~/work/ws1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    env.wait_for_agents(1).await;
    let target = AgentTarget::Workspace {
        path: env.ws("ws1"),
        spawn: false,
    };
    let stream = connect_local(&env.registry(), &target, Path::new(BIN))
        .await
        .unwrap();
    let (client, mut notes) = Client::connect(stream, "test").await.unwrap();

    let spawned = client.spawn_agent(env.ws("ws2")).await.unwrap();
    assert_eq!(spawned.workspace, env.ws("ws2"));
    let again = client.spawn_agent("~/work/ws2").await.unwrap();
    assert_eq!(again.instance, spawned.instance);
    let err = client
        .spawn_agent(env.home().join("outside"))
        .await
        .unwrap_err();
    assert_eq!(
        err.api().unwrap().code,
        loom_client::proto::ErrorCode::BadRequest
    );
    let err = client
        .spawn_agent(env.home().join("missing"))
        .await
        .unwrap_err();
    assert_eq!(
        err.api().unwrap().code,
        loom_client::proto::ErrorCode::NotFound
    );

    let agents = client.list_agents().await.unwrap();
    assert_eq!(agents.len(), 2);
    let sibling = AgentTarget::Instance(spawned.instance.0.clone());
    let stream = connect_local(&env.registry(), &sibling, Path::new(BIN))
        .await
        .unwrap();
    let (sibling_client, _n) = Client::connect(stream, "test").await.unwrap();
    assert_eq!(sibling_client.agent().workspace, env.ws("ws2"));

    client.stop_agent(&spawned.instance).await.unwrap();
    assert_eq!(client.list_agents().await.unwrap().len(), 1);
    let err = client.stop_agent(&spawned.instance).await.unwrap_err();
    assert_eq!(
        err.api().unwrap().code,
        loom_client::proto::ErrorCode::NotFound
    );

    let own = client.agent().instance.clone();
    client.stop_agent(&own).await.unwrap();
    let mut saw_shutdown = false;
    while let Ok(Some(note)) = tokio::time::timeout(Duration::from_secs(10), notes.recv()).await {
        if note == loom_client::Notification::Notice(loom_client::proto::Notice::ShuttingDown) {
            saw_shutdown = true;
        }
    }
    assert!(saw_shutdown);
    assert!(server.wait().unwrap().success());
    assert!(env.ps().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_report_their_telemetry_export() {
    use loom_client::proto::TelemetryStatus;

    let env = Env::new();
    std::fs::create_dir_all(env.config_path().parent().unwrap()).unwrap();
    std::fs::write(
        env.config_path(),
        "allowed_roots:\n- ~/work\ntelemetry:\n  enabled: true\n  endpoint: http://127.0.0.1:9\n",
    )
    .unwrap();
    let serve = |ws: &str, protocol: Option<&str>| {
        let mut command = env.command(&["serve", "--workspace", ws]);
        if let Some(protocol) = protocol {
            command.env("OTEL_EXPORTER_OTLP_PROTOCOL", protocol);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let mut exporting = serve("~/work/ws1", None);
    let mut failed = serve("~/work/ws2", Some("carrier-pigeon"));
    env.wait_for_agents(2).await;
    let status = |ws: &'static str| {
        let registry = env.registry();
        let target = AgentTarget::Workspace {
            path: env.ws(ws),
            spawn: false,
        };
        async move {
            let stream = connect_local(&registry, &target, Path::new(BIN))
                .await
                .unwrap();
            let (client, _notes) = Client::connect(stream, "test").await.unwrap();
            client.telemetry_status().await.unwrap()
        }
    };

    match status("ws1").await {
        TelemetryStatus::Exporting {
            endpoint,
            protocol,
            signals,
        } => {
            assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:9"));
            assert_eq!(protocol, "http/protobuf");
            let names: Vec<_> = signals.iter().map(|s| s.signal.as_str()).collect();
            assert_eq!(names, ["metrics", "logs"]);
        }
        other => panic!("{other:?}"),
    }
    match status("ws2").await {
        TelemetryStatus::Failed { error } => {
            assert!(error.contains("OTEL_EXPORTER_OTLP_PROTOCOL"), "{error}")
        }
        other => panic!("{other:?}"),
    }

    assert!(env.run(&["stop", "--all"]).status.success());
    assert!(exporting.wait().unwrap().success());
    assert!(failed.wait().unwrap().success());
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_telemetry_prints_the_organization_settings() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let env = Env::new();
    let server = MockServer::start().await;
    std::fs::create_dir_all(env.config_path().parent().unwrap()).unwrap();
    std::fs::write(
        env.config_path(),
        format!(
            "version: 1\nllm:\n  providers:\n    anthropic:\n      base_url: {}\n",
            server.uri()
        ),
    )
    .unwrap();
    let fetch = |args: &'static [&'static str]| {
        let command = env.command(args);
        async move {
            let mut command = command;
            tokio::task::spawn_blocking(move || command.output().unwrap())
                .await
                .unwrap()
        }
    };

    let output = fetch(&["config", "fetch-telemetry"]).await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("sign in to Anthropic"),
        "{output:?}"
    );

    let store = loom_llm::CredentialStore::in_dir(&env.home().join(".local/share/loom"));
    store
        .update(|c| {
            c.set_section(
                "anthropic",
                Some(serde_json::json!({
                    "access_token": "sk-ant-oat-access", "refresh_token": "refresh",
                    "expires_at": loom_llm::now_unix() + 3600,
                })),
            )
        })
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/api/claude_code/settings"))
        .and(header("authorization", "Bearer sk-ant-oat-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "checksum": "abc",
            "settings": {"env": {
                "CLAUDE_CODE_ENABLE_TELEMETRY": "1",
                "OTEL_METRICS_EXPORTER": "otlp",
                "OTEL_LOGS_EXPORTER": "otlp",
                "OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.example.com",
                "OTEL_EXPORTER_OTLP_HEADERS": "authorization=Bearer abc",
                "CLAUDE_CODE_OTEL_HEADERS_HELPER": "/usr/local/bin/otel-token",
            }},
        })))
        .mount(&server)
        .await;

    let output = fetch(&["config", "fetch-telemetry"]).await;
    assert!(output.status.success(), "{output:?}");
    let yaml = String::from_utf8(output.stdout.clone()).unwrap();
    assert!(yaml.contains("telemetry:\n  enabled: true\n"), "{yaml}");
    assert!(
        yaml.contains("endpoint: https://otel.example.com"),
        "{yaml}"
    );
    assert!(yaml.contains("authorization: Bearer abc"), "{yaml}");
    assert!(
        stderr(&output).contains(
            "note: CLAUDE_CODE_OTEL_HEADERS_HELPER=\"/usr/local/bin/otel-token\" is not supported"
        ),
        "{output:?}"
    );
    // The printed section is a valid agent.yaml.
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    std::fs::write(env.config_path(), format!("{config}{yaml}")).unwrap();
    let output = fetch(&["config", "check"]).await;
    assert!(output.status.success(), "{output:?}");

    let output = fetch(&["config", "fetch-telemetry", "--raw"]).await;
    assert!(output.status.success(), "{output:?}");
    let raw: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(raw["env"]["OTEL_LOGS_EXPORTER"], "otlp");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_asks_agents_to_read_their_config_again() {
    let env = Env::new();
    // The agent reads its own file; the reload command only needs the
    // registry, so a broken agent file does not stop it from asking.
    let agent_config = env.root.join("agent.yaml");
    std::fs::write(&agent_config, "allowed_roots:\n- ~/work\n").unwrap();
    let mut server = env
        .command(&[
            "serve",
            "--config",
            &path_arg(&agent_config),
            "--workspace",
            "~/work/ws1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let agents = env.wait_for_agents(1).await;

    let output = env.run(&["reload", "--all"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{output:?}");
    assert!(stdout.contains(&agents[0].instance), "{stdout}");
    assert!(stdout.contains("unchanged"), "{stdout}");

    std::fs::write(&agent_config, "allowed_roots:\n- ~/work\nbogus: 1\n").unwrap();
    let output = env.run(&["reload", "--workspace", "~/work/ws1"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr(&output).contains("bogus"), "{}", stderr(&output));

    let output = env.run(&["reload", "--workspace", "~/work/ws2"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("no agent is running"),
        "{}",
        stderr(&output)
    );

    let stopped = env.run(&["stop", "--all"]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(server.wait().unwrap().success());
}
