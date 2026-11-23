use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loom_client::oracle_proto::{Codespace, CodespaceState, Repository, Role};
use loom_client::{Client, ClientIdentity, NewCodespace, OracleClient, RelayHost, pair_oracle};
use loom_config::{Dirs, HostConfig, OracleConfig};
use loom_host::{AgentCache, Host, JoinOptions, ProcessDriver, join};
use loom_oracle::Oracle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const BIN: &str = env!("CARGO_BIN_EXE_loom-agent");
const WAIT: Duration = Duration::from_secs(60);

fn dirs_in(root: &Path) -> Dirs {
    let home = root.join("home");
    Dirs {
        config_home: home.join(".config"),
        data_home: home.join(".local/share"),
        state_home: home.join(".local/state"),
        runtime_dir: None,
        home,
    }
}

async fn git(dir: &Path, args: &[&str]) {
    let status = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdout(std::process::Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

async fn serve_files(root: PathBuf) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let root = root.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let target = text.split_whitespace().nth(1).unwrap_or("/");
                let path = target.split('?').next().unwrap_or_default();
                let file = root.join(path.trim_start_matches('/'));
                let body = (!path.contains("..")).then(|| std::fs::read(&file).ok());
                let response = match body.flatten() {
                    Some(body) => {
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(&body);
                        response
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

struct Agents(PathBuf);

impl Drop for Agents {
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.0) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(bytes) = std::fs::read(entry.path().join("instance.json")) else {
                continue;
            };
            let Ok(file) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if let Some(pid) = file["pid"].as_i64() {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .status();
            }
        }
    }
}

async fn wait_codespace(
    client: &OracleClient,
    id: &str,
    check: impl Fn(&Codespace) -> bool,
) -> Codespace {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let codespace = client
            .codespaces()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap();
        if check(&codespace) {
            return codespace;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the codespace did not reach the state: {codespace:?}"
        );
        assert!(
            codespace.state != CodespaceState::Failed,
            "the codespace failed: {codespace:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn connect_agent(
    url: &str,
    codespace: &Codespace,
    identity: &ClientIdentity,
) -> loom_client::Result<Client> {
    let agent = codespace.agent.as_ref().unwrap();
    let relay = RelayHost::from_parts(url, &agent.host_id, &agent.host_key, None)?;
    let stream = relay.connect(None, identity).await?;
    Ok(Client::connect(stream, "test").await?.0)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_starts_a_codespace_that_a_device_reaches_through_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();

    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--quiet", "-b", "main"]).await;
    std::fs::write(origin.join("README.md"), "hello\n").unwrap();
    git(&origin, &["add", "README.md"]).await;
    git(&origin, &["commit", "--quiet", "-m", "first"]).await;
    let public = root.join("public");
    std::fs::create_dir_all(&public).unwrap();
    git(
        &public,
        &["clone", "--quiet", "--bare", "../origin", "app.git"],
    )
    .await;
    git(&public.join("app.git"), &["update-server-info"]).await;
    let files = serve_files(public).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}");
    let config = OracleConfig {
        url: Some(url.clone()),
        listen: addr.to_string(),
        ..Default::default()
    };
    let oracle = Oracle::open(&config, &dirs_in(&root.join("oracle"))).unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(loom_oracle::serve(
        oracle.clone(),
        listener,
        shutdown.clone(),
    ));

    let laptop = ClientIdentity::generate().unwrap();
    let link = oracle
        .create_pairing(Role::Device, Duration::from_secs(60), None)
        .unwrap()
        .link;
    let mut endpoint = None;
    for _ in 0..100 {
        match pair_oracle(&link, &laptop, "laptop", None).await {
            Ok(paired) => {
                endpoint = Some(paired);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let endpoint = endpoint.expect("cannot pair with the oracle");
    let (device, _events) = OracleClient::connect(&endpoint, &laptop, "test")
        .await
        .unwrap();

    let host_dir = root.join("host");
    let host_link = device.create_host_pairing(None).await.unwrap().link;
    let joined = join(
        &host_dir,
        JoinOptions {
            link: host_link,
            name: "box".into(),
            access_id_env: None,
            access_secret_env: None,
            force: false,
        },
    )
    .await
    .unwrap();
    let codespaces = root.join("cs");
    let _agents = Agents(codespaces.clone());
    let host = Host {
        config: HostConfig::default(),
        oracle: joined,
        identity: Arc::new(loom_host::identity(&host_dir).unwrap()),
        driver: Arc::new(ProcessDriver::new(codespaces.clone())),
        agents: Arc::new(AgentCache::new(
            Some(PathBuf::from(BIN)),
            root.join("agents"),
        )),
        http: loom_github::Http::new().unwrap(),
    };
    tokio::spawn(host.run(shutdown.clone()));
    for _ in 0..200 {
        let hosts = device.hosts().await.unwrap();
        if hosts.iter().any(|h| h.connected && h.report.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    device
        .create_workspace(
            "acme",
            "box",
            vec![Repository {
                url: format!("{files}/app.git"),
                provider: None,
                directory: None,
                base_branch: None,
            }],
        )
        .await
        .unwrap();
    let created = device
        .create_codespace(NewCodespace {
            workspace: "acme".into(),
            cpus: Some(1),
            memory_mib: Some(512),
            ..Default::default()
        })
        .await
        .unwrap();
    let ready = wait_codespace(&device, &created.id, |c| c.state == CodespaceState::Ready).await;
    let instance_dir = codespaces.join(&created.instance);
    assert!(
        !instance_dir
            .join("home/.config/loom/bootstrap.json")
            .exists()
    );

    let agent = connect_agent(&url, &ready, &laptop).await.unwrap();
    let clone = instance_dir.join("workspaces/app");
    assert_eq!(agent.agent().workspace, clone);
    let head = std::fs::read_to_string(clone.join(".git/HEAD")).unwrap();
    assert_eq!(head.trim(), format!("ref: refs/heads/{}", created.branch));
    assert!(clone.join("README.md").exists());

    let phone = ClientIdentity::generate().unwrap();
    assert!(connect_agent(&url, &ready, &phone).await.is_err());
    let phone_link = device.create_device_pairing(None).await.unwrap().link;
    pair_oracle(&phone_link, &phone, "phone", None)
        .await
        .unwrap();
    let mut reached = false;
    for _ in 0..100 {
        if connect_agent(&url, &ready, &phone).await.is_ok() {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reached, "the new device did not reach the agent");

    device.close_codespace(&created.id).await.unwrap();
    wait_codespace(&device, &created.id, |c| c.state == CodespaceState::Closed).await;
    assert!(!instance_dir.exists());
    assert!(connect_agent(&url, &ready, &laptop).await.is_err());
    shutdown.cancel();
}
