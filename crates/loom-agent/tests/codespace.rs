use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loom_client::oracle_proto::{
    Codespace, CodespaceState, GitCredentialInput, GitProviderInput, ProviderRoute, Repository,
    Role, Secret,
};
use loom_client::{Client, ClientIdentity, NewCodespace, OracleClient, RelayHost, pair_oracle};
use loom_config::{Dirs, HostConfig, OracleConfig};
use loom_host::{AgentCache, Host, JoinOptions, ProcessDriver, join};
use loom_oracle::Oracle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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

async fn git_in(home: &Path, dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .output()
        .await
        .unwrap()
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

async fn serve_files(root: PathBuf, auth: Option<(&str, &str)>) -> String {
    let expected = auth.map(|(user, password)| {
        format!(
            "authorization: basic {}",
            base64(format!("{user}:{password}").as_bytes())
        )
        .to_lowercase()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let root = root.clone();
            let expected = expected.clone();
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
                let allowed = expected.as_ref().is_none_or(|expected| {
                    text.lines()
                        .any(|line| line.trim().to_lowercase() == *expected)
                });
                let target = text.split_whitespace().nth(1).unwrap_or("/");
                let path = target.split('?').next().unwrap_or_default();
                let file = root.join(path.trim_start_matches('/'));
                let body = (!path.contains("..")).then(|| std::fs::read(&file).ok());
                let response = match body.flatten() {
                    _ if !allowed => b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
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

async fn bare_repository(root: &Path, served: &Path, name: &str, file: &str) {
    let origin = root.join(format!("origin-{}", name.replace('/', "-")));
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--quiet", "-b", "main"]).await;
    std::fs::write(origin.join(file), "hello\n").unwrap();
    git(&origin, &["add", file]).await;
    git(&origin, &["commit", "--quiet", "-m", "first"]).await;
    let target = served.join(format!("{name}.git"));
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    git(
        root,
        &[
            "clone",
            "--quiet",
            "--bare",
            origin.to_str().unwrap(),
            target.to_str().unwrap(),
        ],
    )
    .await;
    git(&target, &["update-server-info"]).await;
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

struct Env {
    url: String,
    shutdown: CancellationToken,
    laptop: ClientIdentity,
    device: OracleClient,
    codespaces: PathBuf,
    _agents: Agents,
}

async fn start(root: &Path) -> Env {
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
    let agents = Agents(codespaces.clone());
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
    Env {
        url,
        shutdown,
        laptop,
        device,
        codespaces,
        _agents: agents,
    }
}

fn repository(url: String) -> Repository {
    Repository {
        url,
        provider: None,
        directory: None,
        base_branch: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_starts_a_codespace_that_a_device_reaches_through_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let public = root.join("public");
    bare_repository(&root, &public, "app", "README.md").await;
    let files = serve_files(public, None).await;
    let env = start(&root).await;
    let (device, url, laptop) = (&env.device, &env.url, &env.laptop);

    device
        .create_workspace("acme", "box", vec![repository(format!("{files}/app.git"))])
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
    let ready = wait_codespace(device, &created.id, |c| c.state == CodespaceState::Ready).await;
    let instance_dir = env.codespaces.join(&created.instance);
    assert!(
        !instance_dir
            .join("home/.config/loom/bootstrap.json")
            .exists()
    );

    let agent = connect_agent(url, &ready, laptop).await.unwrap();
    let clone = instance_dir.join("workspaces/app");
    assert_eq!(agent.agent().workspace, clone);
    let head = std::fs::read_to_string(clone.join(".git/HEAD")).unwrap();
    assert_eq!(head.trim(), format!("ref: refs/heads/{}", created.branch));
    assert!(clone.join("README.md").exists());

    let phone = ClientIdentity::generate().unwrap();
    assert!(connect_agent(url, &ready, &phone).await.is_err());
    let phone_link = device.create_device_pairing(None).await.unwrap().link;
    pair_oracle(&phone_link, &phone, "phone", None)
        .await
        .unwrap();
    let mut reached = false;
    for _ in 0..100 {
        if connect_agent(url, &ready, &phone).await.is_ok() {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reached, "the new device did not reach the agent");

    device.close_codespace(&created.id).await.unwrap();
    wait_codespace(device, &created.id, |c| c.state == CodespaceState::Closed).await;
    assert!(!instance_dir.exists());
    assert!(connect_agent(url, &ready, laptop).await.is_err());
    env.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codespace_clones_private_repositories_with_the_credentials_of_its_preset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let private = root.join("private");
    let public = root.join("public");
    bare_repository(&root, &private, "acme/app", "app.txt").await;
    bare_repository(&root, &public, "lib", "lib.txt").await;
    let private_url = serve_files(private, Some(("octo", "secret-token"))).await;
    let public_url = serve_files(public, None).await;
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"login": "octo", "id": 7})),
        )
        .mount(&api)
        .await;
    let env = start(&root).await;
    let device = &env.device;

    device
        .set_git_provider(GitProviderInput {
            name: "files".into(),
            web_url: private_url.clone(),
            api_url: Some(api.uri()),
            git_name: Some("Dev".into()),
            git_email: Some("dev@example.com".into()),
            route: ProviderRoute::Direct,
        })
        .await
        .unwrap();
    let credential = device
        .set_git_credential(GitCredentialInput {
            name: "files-token".into(),
            provider: "files".into(),
            owner: None,
            repository: None,
            token: Some(Secret::new("secret-token")),
        })
        .await
        .unwrap();
    assert_eq!(credential.login.as_deref(), Some("octo"));
    device
        .set_preset("dev", "git:\n  credentials: [files-token]\n")
        .await
        .unwrap();
    let workspace = device
        .create_workspace(
            "acme",
            "box",
            vec![
                repository(format!("{private_url}/acme/app.git")),
                repository(format!("{public_url}/lib.git")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(workspace.repositories[0].provider.as_deref(), Some("files"));
    assert_eq!(workspace.repositories[1].provider, None);
    let created = device
        .create_codespace_with_warnings(NewCodespace {
            workspace: "acme".into(),
            preset: Some("dev".into()),
            cpus: Some(1),
            memory_mib: Some(512),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.warnings, Vec::<String>::new());
    let ready = wait_codespace(device, &created.codespace.id, |c| {
        c.state == CodespaceState::Ready
    })
    .await;
    let instance_dir = env.codespaces.join(&ready.instance);
    let home = instance_dir.join("home");
    let agent = connect_agent(&env.url, &ready, &env.laptop).await.unwrap();
    assert_eq!(
        agent.agent().workspace,
        instance_dir.join("workspaces/acme")
    );
    let app = instance_dir.join("workspaces/acme/app");
    let lib = instance_dir.join("workspaces/acme/lib");
    assert!(app.join("app.txt").exists());
    assert!(lib.join("lib.txt").exists());
    for clone in [&app, &lib] {
        let head = std::fs::read_to_string(clone.join(".git/HEAD")).unwrap();
        assert_eq!(head.trim(), format!("ref: refs/heads/{}", ready.branch));
    }

    let config = |clone: PathBuf, key: &'static str| {
        let home = home.clone();
        async move {
            let output = git_in(&home, &clone, &["config", "--local", "--get", key]).await;
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }
    };
    assert_eq!(config(app.clone(), "user.name").await, "Dev");
    assert_eq!(config(app.clone(), "user.email").await, "dev@example.com");
    assert_eq!(config(lib.clone(), "user.name").await, "");
    let hosts_file = home.join(".config/gh/hosts.yml");
    let hosts = std::fs::read_to_string(&hosts_file).unwrap();
    assert!(hosts.contains("secret-token"), "{hosts}");
    assert!(hosts.contains("octo"), "{hosts}");
    let listed = git_in(&home, &app, &["ls-remote", "origin"]).await;
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    std::fs::write(app.join("new.txt"), "new\n").unwrap();
    assert!(
        git_in(&home, &app, &["add", "new.txt"])
            .await
            .status
            .success()
    );
    let committed = git_in(&home, &app, &["commit", "--quiet", "-m", "change"]).await;
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    let author = git_in(&home, &app, &["log", "-1", "--format=%an <%ae>"]).await;
    assert_eq!(
        String::from_utf8_lossy(&author.stdout).trim(),
        "Dev <dev@example.com>"
    );

    device.set_preset("dev", "git: {}\n").await.unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    while std::fs::read_to_string(&hosts_file)
        .unwrap()
        .contains("secret-token")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the agent did not remove the token of gh"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let refused = git_in(&home, &app, &["ls-remote", "origin"]).await;
    assert!(!refused.status.success());

    device.close_codespace(&ready.id).await.unwrap();
    wait_codespace(device, &ready.id, |c| c.state == CodespaceState::Closed).await;
    env.shutdown.cancel();
}
