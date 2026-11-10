use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use loom_client::{
    AgentTarget, Client, ClientError, ClientIdentity, Registry, RelayHost, connect_local,
    pair_with_uri,
};
use loom_config::RelayServerConfig;
use tokio_util::sync::CancellationToken;

const BIN: &str = env!("CARGO_BIN_EXE_loom-agent");

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home/work/ws1")).unwrap();
        Self { _dir: dir, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
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

    fn registry(&self) -> Registry {
        Registry::new(self.root.join("run/loom"))
    }
}

fn stdout(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

async fn start_relay(allowed: String) -> (String, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = RelayServerConfig {
        allowed_hosts: vec![allowed],
        ..Default::default()
    };
    let shutdown = CancellationToken::new();
    tokio::spawn(loom_relay::serve(
        loom_relay::Relay::new(config),
        listener,
        shutdown.clone(),
    ));
    (format!("ws://{addr}"), shutdown)
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_reach_the_agent_through_the_relay() {
    let env = Env::new();
    let host: serde_json::Value =
        serde_json::from_str(&stdout(&env.run(&["host", "--json"]))).unwrap();
    let host_id = host["host_id"].as_str().unwrap().to_owned();
    let static_key = host["static_key"].as_str().unwrap().to_owned();
    let (url, relay_shutdown) = start_relay(host_id.clone()).await;
    let config_dir = env.home().join(".config/loom");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("agent.yaml"),
        format!("allowed_roots:\n- ~/work\nrelay:\n  enabled: true\n  url: {url}\n"),
    )
    .unwrap();
    let no_agent = env.run(&["pair", "--no-wait", "--no-qr"]);
    assert!(!no_agent.status.success());

    let mut server = env
        .command(&["serve", "--workspace", "~/work/ws1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut entries = Vec::new();
    for _ in 0..400 {
        entries = env.registry().list().unwrap();
        if !entries.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(entries.len(), 1);

    let target = AgentTarget::Workspace {
        path: env.home().join("work/ws1"),
        spawn: false,
    };
    let local = connect_local(&env.registry(), &target, Path::new(BIN))
        .await
        .unwrap();
    let (local, _notes) = Client::connect(local, "local").await.unwrap();
    let info = local.host_key().await.unwrap();
    assert_eq!(info.host.to_string(), host_id);
    assert_eq!(info.relay_url.as_deref(), Some(url.as_str()));

    let laptop = ClientIdentity::generate().unwrap();
    let relay_host = RelayHost::from_parts(&url, &host_id, &static_key, None).unwrap();
    assert_eq!(relay_host.list_agents().await.unwrap().len(), 1);
    let denied = relay_host.connect(None, &laptop).await.unwrap_err();
    assert!(denied.to_string().contains("not authorized"), "{denied}");
    local
        .authorize_client(laptop.public_key(), "laptop")
        .await
        .unwrap();
    let stream = relay_host.connect(None, &laptop).await.unwrap();
    let (remote, _notes) = Client::connect(stream, "remote").await.unwrap();
    assert_eq!(remote.agent().workspace, env.home().join("work/ws1"));
    let session = remote.create_session(None).await.unwrap();
    assert_eq!(local.list_sessions().await.unwrap()[0].id, session.id);

    let uri = stdout(&env.run(&["pair", "--no-wait", "--no-qr"]));
    let uri = uri
        .lines()
        .find(|l| l.starts_with("loom+ws://"))
        .unwrap()
        .to_owned();
    let phone = ClientIdentity::generate().unwrap();
    let (paired, instance) = pair_with_uri(&uri, &phone, "phone", None).await.unwrap();
    assert_eq!(paired.host_key_text(), static_key);
    let stream = paired.connect(Some(&instance), &phone).await.unwrap();
    let (phone_client, _notes) = Client::connect(stream, "phone").await.unwrap();
    let labels: Vec<String> = phone_client
        .list_clients()
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.label)
        .collect();
    assert_eq!(labels, ["laptop", "phone"]);

    phone_client.revoke_client("laptop").await.unwrap();
    let mut closed = false;
    for _ in 0..100 {
        match remote.list_sessions().await {
            Err(ClientError::Closed) => {
                closed = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(closed, "the revoked client stayed connected");
    let listed = stdout(&env.run(&["clients", "list"]));
    assert!(
        listed.contains("phone") && !listed.contains("laptop"),
        "{listed}"
    );
    assert!(env.run(&["clients", "revoke", "phone"]).status.success());
    assert!(!env.run(&["clients", "revoke", "phone"]).status.success());

    assert!(env.run(&["stop", "--all"]).status.success());
    assert!(server.wait().unwrap().success());
    relay_shutdown.cancel();
}
