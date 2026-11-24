use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_loom-agent");

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { _dir: dir, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn config_path(&self) -> PathBuf {
        self.home().join(".config/loom/agent.yaml")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.home())
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("LOOM_LOG", "info")
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn the_agent_runs_only_as_the_agent_of_a_codespace() {
    let env = Env::new();
    for args in [
        &["serve"][..],
        &["serve", "--workspace", "/tmp"],
        &["stdio"],
        &["stdio", "--workspace", "/tmp"],
        &["ps"],
        &["stop", "--all"],
        &["reload", "--all"],
        &["pair"],
        &["clients", "list"],
        &["host"],
        &["update"],
    ] {
        let output = env.run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
}

#[test]
fn config_commands_work_without_an_agent() {
    let env = Env::new();
    let output = env.run(&["config", "check"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("does not exist"), "{output:?}");
    let output = env.run(&["config", "init"]);
    assert!(output.status.success(), "{output:?}");
    let output = env.run(&["config", "check"]);
    assert!(output.status.success(), "{output:?}");
    let output = env.run(&["config", "path"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        env.config_path().display().to_string()
    );
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
