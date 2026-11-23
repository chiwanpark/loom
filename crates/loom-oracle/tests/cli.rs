use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use loom_client::oracle_proto::Role;
use loom_client::{ClientIdentity, OracleClient, pair_oracle};
use loom_config::{Dirs, ORACLE_MASTER_KEY_FILE, OracleConfig};
use loom_oracle::{Oracle, master_key_path};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const ORACLE: &str = env!("CARGO_BIN_EXE_loom-oracle");
const WAIT: Duration = Duration::from_secs(30);

fn dirs_in(home: &Path) -> Dirs {
    Dirs {
        config_home: home.join(".config"),
        data_home: home.join(".local/share"),
        state_home: home.join(".local/state"),
        runtime_dir: None,
        home: home.to_path_buf(),
    }
}

fn oracle_cmd(home: &Path) -> Command {
    let mut command = Command::new(ORACLE);
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env_remove("LOOM_ORACLE_CONFIG")
        .env_remove("CREDENTIALS_DIRECTORY")
        .kill_on_drop(true);
    command
}

async fn output(home: &Path, args: &[&str]) -> (bool, String, String) {
    let output = tokio::time::timeout(WAIT, oracle_cmd(home).args(args).output())
        .await
        .expect("loom-oracle timed out")
        .unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn config_commands_manage_oracle_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let path = home.join(".config/loom/oracle.yaml");
    let (ok, out, _) = output(home, &["config", "path"]).await;
    assert!(ok);
    assert_eq!(out.trim(), path.display().to_string());
    let (ok, out, _) = output(home, &["config", "init"]).await;
    assert!(ok, "{out}");
    assert!(path.exists());
    let (ok, _, err) = output(home, &["config", "init"]).await;
    assert!(!ok);
    assert!(err.contains("--force"), "{err}");
    let (ok, out, _) = output(home, &["config", "check"]).await;
    assert!(ok);
    assert!(out.ends_with(": ok\n"), "{out}");
    let (ok, out, _) = output(home, &["config", "schema"]).await;
    assert!(ok);
    let schema: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(schema["properties"]["master_key"].is_object());

    std::fs::write(&path, "url: https://wrong\n").unwrap();
    let (ok, _, err) = output(home, &["config", "check"]).await;
    assert!(!ok);
    assert!(err.contains("url"), "{err}");
    let other = home.join("other.yaml");
    std::fs::write(&other, "listen: 127.0.0.1:1\n").unwrap();
    let (ok, _, _) = output(
        home,
        &["--config", other.to_str().unwrap(), "config", "check"],
    )
    .await;
    assert!(ok);
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_shows_a_link_for_the_first_device_and_waits() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config_path = home.join(".config/loom/oracle.yaml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, format!("listen: {addr}\nurl: ws://{addr}\n")).unwrap();
    let dirs = dirs_in(home);
    let config = OracleConfig::load(&config_path, &dirs).unwrap();
    let oracle = Oracle::open(&config, &dirs).unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(loom_oracle::serve(
        oracle.clone(),
        listener,
        shutdown.clone(),
    ));

    let (ok, out, _) = output(home, &["pair", "--no-wait", "--no-qr", "--ttl", "2m"]).await;
    assert!(ok);
    assert!(out.starts_with(&format!("loom+ws://{addr}")), "{out}");
    assert!(out.contains("expires in 2m"), "{out}");
    let (ok, _, err) = output(home, &["pair", "--no-wait", "--ttl", "30d"]).await;
    assert!(!ok);
    assert!(err.contains("seconds"), "{err}");

    let mut pair = oracle_cmd(home)
        .args(["pair", "--no-qr"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(pair.stdout.take().unwrap()).lines();
    let link = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let identity = ClientIdentity::generate().unwrap();
    let endpoint = pair_oracle(&link, &identity, "laptop", None).await.unwrap();
    let mut rest = Vec::new();
    while let Some(line) = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
    {
        rest.push(line);
    }
    assert!(pair.wait().await.unwrap().success());
    assert_eq!(rest.last().map(String::as_str), Some("paired with laptop"));

    let (client, _events) = OracleClient::connect(&endpoint, &identity, "test")
        .await
        .unwrap();
    assert_eq!(client.hello().peer.role, Role::Device);
    assert_eq!(client.pairings().await.unwrap().len(), 1);
    let log = oracle.store().audit_log(10).unwrap();
    assert!(
        log.iter()
            .any(|row| row.detail.contains("loom-oracle pair"))
    );
    shutdown.cancel();
}

#[test]
fn the_master_key_can_be_a_systemd_credential() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = dirs_in(dir.path());
    let credentials = dir.path().join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    let default = OracleConfig::default();
    let data_key = dirs
        .data_home
        .join("loom/oracle")
        .join(ORACLE_MASTER_KEY_FILE);
    assert_eq!(master_key_path(&default, &dirs, None), data_key);
    assert_eq!(
        master_key_path(&default, &dirs, Some(&credentials)),
        data_key
    );
    std::fs::write(credentials.join(ORACLE_MASTER_KEY_FILE), "x").unwrap();
    assert_eq!(
        master_key_path(&default, &dirs, Some(&credentials)),
        credentials.join(ORACLE_MASTER_KEY_FILE)
    );
    let named = OracleConfig {
        master_key: Some(PathBuf::from("/etc/loom-oracle/master.key")),
        ..Default::default()
    };
    assert_eq!(
        master_key_path(&named, &dirs, Some(&credentials)),
        PathBuf::from("/etc/loom-oracle/master.key")
    );
}
