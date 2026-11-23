use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use loom_config::{Dirs, OracleConfig};
use loom_oracle::Oracle;
use loom_oracle_proto::Role;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const CLI: &str = env!("CARGO_BIN_EXE_loom-oracle-cli");

struct TestOracle {
    _dir: tempfile::TempDir,
    oracle: Arc<Oracle>,
    shutdown: CancellationToken,
}

async fn start_oracle() -> TestOracle {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let dirs = Dirs {
        config_home: home.join(".config"),
        data_home: home.join(".local/share"),
        state_home: home.join(".local/state"),
        runtime_dir: None,
        home,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = OracleConfig {
        url: Some(format!("ws://{addr}")),
        listen: addr.to_string(),
        ..Default::default()
    };
    let oracle = Oracle::open(&config, &dirs).unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(loom_oracle::serve(
        oracle.clone(),
        listener,
        shutdown.clone(),
    ));
    for _ in 0..200 {
        if oracle.relay().online_agents() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    TestOracle {
        _dir: dir,
        oracle,
        shutdown,
    }
}

fn cli(state: &Path) -> Command {
    let mut command = Command::new(CLI);
    command
        .env(
            "LOOM_ORACLE_CLI_STATE",
            state.to_str().expect("the test path is UTF-8"),
        )
        .env("HOME", state)
        .kill_on_drop(true);
    command
}

async fn run(state: &Path, args: &[&str]) -> Output {
    tokio::time::timeout(Duration::from_secs(30), cli(state).args(args).output())
        .await
        .expect("the CLI timed out")
        .unwrap()
}

async fn ok(state: &Path, args: &[&str]) -> String {
    let output = run(state, args).await;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

async fn json(state: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&ok(state, &all).await).unwrap()
}

async fn fails(state: &Path, args: &[&str]) -> String {
    let output = run(state, args).await;
    assert!(!output.status.success(), "{args:?} succeeded");
    String::from_utf8(output.stderr).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_drives_the_control_service() {
    let oracle = start_oracle().await;
    let laptop = tempfile::tempdir().unwrap();
    let phone = tempfile::tempdir().unwrap();
    let (laptop, phone) = (laptop.path(), phone.path());

    let err = fails(laptop, &["devices"]).await;
    assert!(err.contains("no oracle is paired"), "{err}");

    let first = oracle
        .oracle
        .create_pairing(Role::Device, Duration::from_secs(60), None)
        .unwrap();
    let out = ok(laptop, &["pair", &first.link, "--label", "laptop"]).await;
    assert!(out.contains("as laptop"), "{out}");
    let err = fails(phone, &["pair", &first.link, "--label", "phone"]).await;
    assert!(err.contains("unknown or expired"), "{err}");

    let info = json(laptop, &["info"]).await;
    assert_eq!(info["peer"]["label"], "laptop");
    assert_eq!(info["peer"]["role"], "device");
    assert_eq!(
        info["oracle"]["host_id"],
        oracle.oracle.identity().host_id().to_string()
    );
    let text = ok(laptop, &["info"]).await;
    assert!(text.contains("peer 1 laptop (device)"), "{text}");

    let mut watch = cli(laptop)
        .args(["--json", "watch", "--count", "50"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let subscribed = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(subscribed.contains("subscribed"), "{subscribed}");

    let link = json(laptop, &["pair-device", "--ttl", "5m"]).await;
    assert_eq!(link["pairing"]["role"], "device");
    assert_eq!(
        link["pairing"]["expires_at"].as_u64().unwrap()
            - link["pairing"]["created_at"].as_u64().unwrap(),
        300
    );
    let text = ok(laptop, &["pairings"]).await;
    assert!(
        text.contains(link["pairing"]["id"].as_str().unwrap()),
        "{text}"
    );
    ok(
        phone,
        &["pair", link["link"].as_str().unwrap(), "--label", "phone"],
    )
    .await;

    let mut events = Vec::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let event = serde_json::from_str::<Value>(&line).unwrap();
        let done = event["type"] == "device_changed" && event["device"]["label"] == "phone";
        events.push(event);
        if done {
            break;
        }
    }
    watch.kill().await.unwrap();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .filter(|kind| *kind != "device_changed")
        .collect();
    assert_eq!(kinds, ["pairing_changed", "pairing_removed"]);
    let removed = events.iter().find(|e| e["type"] == "pairing_removed");
    assert_eq!(removed.unwrap()["used"], true);
    assert_eq!(events.last().unwrap()["device"]["connected"], false);

    ok(phone, &["push-token", "token-1"]).await;
    let devices = json(laptop, &["devices"]).await;
    let devices = devices.as_array().unwrap();
    assert_eq!(devices.len(), 2);
    let phone_id = devices[1]["id"].as_u64().unwrap();
    assert_eq!(devices[1]["push"], true);
    let text = ok(laptop, &["devices"]).await;
    assert!(text.contains("laptop\tthis device"), "{text}");
    assert!(text.contains("push"), "{text}");
    let renamed = json(laptop, &["rename-device", &phone_id.to_string(), "pixel"]).await;
    assert_eq!(renamed["label"], "pixel");
    ok(phone, &["push-token", "--clear"]).await;
    let devices = json(laptop, &["devices"]).await;
    assert_eq!(devices[1]["push"], false);

    let host = "ab".repeat(32);
    let added = json(laptop, &["add-relay-host", &host, "devbox"]).await;
    assert_eq!(added["host_id"], host);
    let text = ok(laptop, &["relay-hosts"]).await;
    assert!(text.contains("devbox\toffline"), "{text}");
    let err = fails(laptop, &["add-relay-host", "nope", "x"]).await;
    assert!(err.contains("not a host ID"), "{err}");
    ok(laptop, &["remove-relay-host", &host]).await;
    assert_eq!(json(laptop, &["relay-hosts"]).await, Value::Array(vec![]));

    let pending = json(laptop, &["pair-device", "--no-qr"]).await;
    let id = pending["pairing"]["id"].as_str().unwrap();
    ok(laptop, &["cancel-pairing", id]).await;
    let err = fails(laptop, &["cancel-pairing", id]).await;
    assert!(err.contains("no pairing"), "{err}");

    let provider = json(
        laptop,
        &[
            "set-provider",
            "ghe",
            "--url",
            "https://ghe.example.com/",
            "--git-email",
            "dev@example.com",
            "--route",
            "hosts",
        ],
    )
    .await;
    assert_eq!(provider["api_url"], "https://ghe.example.com/api/v3");
    assert_eq!(provider["route"], "hosts");
    let text = ok(laptop, &["providers"]).await;
    assert!(text.contains("ghe\thttps://ghe.example.com\t"), "{text}");
    assert!(text.contains("route hosts"), "{text}");
    let preset_file = laptop.join("preset.yaml");
    std::fs::write(&preset_file, "git:\n  credentials: []\n").unwrap();
    let preset_path = preset_file.to_str().unwrap();
    let preset = json(laptop, &["set-preset", "dev", preset_path]).await;
    assert_eq!(preset["git_credentials"], Value::Array(vec![]));
    std::fs::write(&preset_file, "git:\n  credentials: [nope]\n").unwrap();
    let err = fails(laptop, &["set-preset", "dev", preset_path]).await;
    assert!(err.contains("no such Git credential"), "{err}");
    let err = fails(laptop, &["set-credential", "work", "--provider", "ghe"]).await;
    assert!(err.contains("needs a token"), "{err}");
    ok(laptop, &["delete-preset", "dev"]).await;
    ok(laptop, &["delete-provider", "ghe"]).await;
    assert_eq!(json(laptop, &["providers"]).await, Value::Array(vec![]));

    ok(laptop, &["revoke-device", &phone_id.to_string()]).await;
    let err = fails(phone, &["devices"]).await;
    assert!(err.contains("not paired"), "{err}");
    let err = fails(laptop, &["revoke-device", &phone_id.to_string()]).await;
    assert!(err.contains("no device"), "{err}");
    oracle.shutdown.cancel();
}
