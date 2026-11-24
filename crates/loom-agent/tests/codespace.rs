use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use loom_client::oracle_proto::{
    Codespace, CodespaceState, FirstMessage, GitCredentialInput, GitProviderInput, ModelAccount,
    ModelLoginInput, ModelLoginMethod, ModelLoginPrompt, PresetFile, ProviderRoute, Repository,
    Role, Secret,
};
use loom_client::{
    Client, ClientIdentity, CloseGuardError, NewCodespace, OracleClient, RelayHost,
    choices_from_lists, close_plan, pair_oracle,
};
use loom_config::{Dirs, HostConfig, OracleConfig};
use loom_host::{AgentCache, Host, JoinOptions, ProcessDriver, join};
use loom_oracle::Oracle;
use loom_proto::{LoginMethod, PolicyAction, PushRepositories};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path, path_regex};
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

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut request = Vec::new();
    let mut buf = [0u8; 8192];
    let end = loop {
        if let Some(end) = find(&request, b"\r\n\r\n") {
            break end;
        }
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&request[..end]).into_owned();
    let mut body = request[end + 4..].to_vec();
    let lower = head.to_lowercase();
    let length = lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok());
    let chunked = lower.contains("transfer-encoding: chunked");
    let complete = |body: &[u8]| match (length, chunked) {
        (Some(length), _) => body.len() >= length,
        (None, true) => find(body, b"0\r\n\r\n").is_some(),
        (None, false) => true,
    };
    while !complete(&body) {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    if chunked {
        let mut plain = Vec::new();
        let mut rest = &body[..];
        while let Some(line) = find(rest, b"\r\n") {
            let size =
                usize::from_str_radix(std::str::from_utf8(&rest[..line]).ok()?.trim(), 16).ok()?;
            if size == 0 {
                break;
            }
            plain.extend_from_slice(&rest[line + 2..line + 2 + size]);
            rest = &rest[line + 2 + size + 2..];
        }
        body = plain;
    }
    Some((head, body))
}

async fn cgi(root: &Path, head: &str, body: Vec<u8>, user: Option<&str>) -> Vec<u8> {
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or("GET").to_owned();
    let target = first.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let mut command = tokio::process::Command::new("git");
    command
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_TYPE", header("content-type").unwrap_or_default())
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    if let Some(encoding) = header("content-encoding") {
        command.env("HTTP_CONTENT_ENCODING", encoding);
    }
    if let Some(user) = user {
        command.env("REMOTE_USER", user);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let output = child.wait_with_output().await.unwrap();
    let split = find(&output.stdout, b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| find(&output.stdout, b"\n\n").map(|i| (i, 2)));
    let Some((end, gap)) = split else {
        return b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
    };
    let headers = String::from_utf8_lossy(&output.stdout[..end]).into_owned();
    let content = &output.stdout[end + gap..];
    let mut status = "200 OK".to_owned();
    let mut response = String::new();
    for line in headers.lines() {
        match line.split_once(':') {
            Some((key, value)) if key.eq_ignore_ascii_case("status") => {
                status = value.trim().to_owned();
            }
            Some(_) => response.push_str(&format!("{line}\r\n")),
            None => {}
        }
    }
    let mut bytes = format!(
        "HTTP/1.1 {status}\r\n{response}Content-Length: {}\r\nConnection: close\r\n\r\n",
        content.len()
    )
    .into_bytes();
    bytes.extend_from_slice(content);
    bytes
}

async fn serve_git(root: PathBuf, auth: Option<(&str, &str)>) -> String {
    let expected = auth.map(|(user, password)| {
        format!(
            "authorization: basic {}",
            base64(format!("{user}:{password}").as_bytes())
        )
        .to_lowercase()
    });
    let user = auth.map(|(user, _)| user.to_owned());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let root = root.clone();
            let expected = expected.clone();
            let user = user.clone();
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut stream).await else {
                    return;
                };
                let allowed = expected.as_ref().is_none_or(|expected| {
                    head.lines()
                        .any(|line| line.trim().to_lowercase() == *expected)
                });
                let response = if allowed {
                    cgi(&root, &head, body, user.as_deref()).await
                } else {
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                };
                let _ = stream.write_all(&response).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

async fn bare_repository(root: &Path, served: &Path, name: &str, file: &str) {
    bare_repository_with(root, served, name, &[(file, "hello\n")]).await;
}

async fn bare_repository_with(root: &Path, served: &Path, name: &str, files: &[(&str, &str)]) {
    let origin = root.join(format!("origin-{}", name.replace('/', "-")));
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--quiet", "-b", "main"]).await;
    for (file, content) in files {
        let path = origin.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        git(&origin, &["add", file]).await;
    }
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
    oracle: Arc<Oracle>,
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
        driver: Arc::new(ProcessDriver::new(codespaces.clone(), Default::default())),
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
        oracle,
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
    bare_repository_with(
        &root,
        &public,
        "app",
        &[
            ("README.md", "hello\n"),
            (
                ".loom/setup.sh",
                "echo \"$(basename \"$PWD\")\" > setup-ran\n",
            ),
        ],
    )
    .await;
    let files = serve_git(public, None).await;
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
    assert_eq!(
        std::fs::read_to_string(clone.join("setup-ran")).unwrap(),
        "app\n"
    );

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
async fn a_codespace_pushes_and_opens_pull_requests_with_the_credentials_of_its_preset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let private = root.join("private");
    let public = root.join("public");
    bare_repository(&root, &private, "acme/app", "app.txt").await;
    bare_repository(&root, &private, "acme/lib", "lib.txt").await;
    bare_repository(&root, &public, "tools", "tools.txt").await;
    let private_url = serve_git(private.clone(), Some(("octo", "secret-token"))).await;
    let public_url = serve_git(public, None).await;
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"login": "octo", "id": 7})),
        )
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/repos/acme/(app|lib)/pulls$"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&api)
        .await;
    for (name, number) in [("app", 1), ("lib", 2)] {
        let pull = serde_json::json!({
            "number": number,
            "html_url": format!("{private_url}/acme/{name}/pull/{number}"),
            "body": "Body",
        });
        Mock::given(method("POST"))
            .and(path(format!("/repos/acme/{name}/pulls")))
            .respond_with(ResponseTemplate::new(201).set_body_json(pull.clone()))
            .mount(&api)
            .await;
        Mock::given(method("PATCH"))
            .and(path(format!("/repos/acme/{name}/pulls/{number}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(pull))
            .mount(&api)
            .await;
    }
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
            app_id: None,
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
                repository(format!("{private_url}/acme/lib.git")),
                repository(format!("{public_url}/tools.git")),
            ],
        )
        .await
        .unwrap();
    assert_eq!(workspace.repositories[0].provider.as_deref(), Some("files"));
    assert_eq!(workspace.repositories[1].provider.as_deref(), Some("files"));
    assert_eq!(workspace.repositories[2].provider, None);
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
    let tools = instance_dir.join("workspaces/acme/tools");
    assert!(app.join("app.txt").exists());
    assert!(lib.join("lib.txt").exists());
    for clone in [&app, &lib, &tools] {
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
    assert_eq!(config(lib.clone(), "user.name").await, "Dev");
    assert_eq!(config(tools.clone(), "user.name").await, "");
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

    std::fs::write(lib.join("more.txt"), "more\n").unwrap();
    let changes = agent.workspace_changes().await.unwrap();
    assert!(changes.repository);
    let paths: Vec<&str> = changes.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["lib/more.txt"]);
    let diff = agent.file_diff("lib/more.txt", None, false).await.unwrap();
    assert_eq!(diff.path, "lib/more.txt");
    assert!(diff.diff.contains("+more"), "{}", diff.diff);
    let err = agent
        .file_diff("other/more.txt", None, false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a git repository"), "{err}");
    let statuses = agent.repositories().await.unwrap();
    let work: Vec<(&str, u32, u32, u32)> = statuses
        .iter()
        .map(|s| (s.directory.as_str(), s.changed_files, s.ahead, s.unpushed))
        .collect();
    assert_eq!(
        work,
        vec![("app", 0, 1, 1), ("lib", 1, 0, 0), ("tools", 0, 0, 0)]
    );
    assert_eq!(statuses[0].base_branch.as_deref(), Some("main"));
    assert!(!statuses[0].pushed);
    let choose = choices_from_lists(&statuses, &["*".into()], &[]).unwrap();
    assert!(matches!(
        close_plan(&statuses, &choose, false),
        Err(CloseGuardError::NeedsMessage(_))
    ));
    let to_push = close_plan(&statuses, &choose, true).unwrap();
    assert_eq!(to_push, vec!["app".to_owned(), "lib".to_owned()]);
    let pushed = agent
        .push_repositories(PushRepositories {
            commit_message: Some("More".into()),
            directories: to_push,
        })
        .await
        .unwrap();
    for status in &pushed {
        assert_eq!(status.error, None, "{status:?}");
        assert!(status.pushed && !status.has_unsaved_work(), "{status:?}");
    }
    for name in ["app", "lib"] {
        let bare = private.join(format!("acme/{name}.git"));
        let branch = format!("refs/heads/{}", ready.branch);
        git(&bare, &["rev-parse", "--verify", &branch]).await;
    }
    let lib_author = git_in(&home, &lib, &["log", "-1", "--format=%an <%ae> %s"]).await;
    assert_eq!(
        String::from_utf8_lossy(&lib_author.stdout).trim(),
        "Dev <dev@example.com> More"
    );

    let opened = agent
        .open_pull_requests("Change", "Body", false)
        .await
        .unwrap();
    assert_eq!(opened.len(), 3);
    for result in &opened[..2] {
        assert_eq!(result.error, None, "{result:?}");
        assert!(result.created);
    }
    assert_eq!(
        opened[2].skipped.as_deref(),
        Some("the repository has no Git provider")
    );
    let requests = api.received_requests().await.unwrap();
    let created: Vec<serde_json::Value> = requests
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(created.len(), 2);
    assert_eq!(created[0]["head"], ready.branch.as_str());
    assert_eq!(created[0]["base"], "main");
    let linked: Vec<String> = requests
        .iter()
        .filter(|r| r.method.as_str() == "PATCH")
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert_eq!(linked.len(), 2);
    assert!(linked[0].contains("acme/lib#2"), "{}", linked[0]);
    assert!(linked[1].contains("acme/app#1"), "{}", linked[1]);
    let listed = agent.repositories().await.unwrap();
    assert!(
        listed[0]
            .pull_request
            .as_deref()
            .unwrap()
            .ends_with("/acme/app/pull/1")
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

fn answer(text: &str) -> ResponseTemplate {
    let events = [
        json!({"type": "message_start", "message": {"id": "m", "model": "claude-test", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ];
    let body: String = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn sign_in(device: &OracleClient, account: &str, code: &str) -> ModelAccount {
    let login = device
        .start_model_login(
            account,
            "anthropic",
            ModelLoginMethod::Browser { redirect_uri: None },
        )
        .await
        .unwrap();
    let ModelLoginPrompt::OpenUrl { url, .. } = login.prompt else {
        panic!("not a browser sign-in: {:?}", login.prompt);
    };
    let state = url
        .split(['?', '&'])
        .find_map(|part| part.strip_prefix("state="))
        .unwrap();
    device
        .complete_model_login(
            &login.login,
            ModelLoginInput::Code {
                code: format!("{code}#{state}"),
            },
        )
        .await
        .unwrap()
}

async fn wait_for<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some(found) = check().await {
            return found;
        }
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn model_request(llm: &MockServer, text: &str) -> wiremock::Request {
    wait_for(
        &format!("no request to the model has {text:?}"),
        async || {
            llm.received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .find(|request| {
                    request.url.path() == "/v1/messages"
                        && String::from_utf8_lossy(&request.body).contains(text)
                })
        },
    )
    .await
}

fn bearer(request: &wiremock::Request) -> String {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codespace_starts_its_first_turn_with_the_model_account_of_its_preset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let public = root.join("public");
    bare_repository(&root, &public, "app", "README.md").await;
    let files = serve_git(public, None).await;
    let llm = MockServer::start().await;
    let token = |code: &str, access: &str, refresh: &str, expires: u64, who: &str| {
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(
                json!({"grant_type": "authorization_code", "code": code}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access, "refresh_token": refresh, "expires_in": expires,
                "account": {"uuid": format!("account-{who}"), "email_address": format!("{who}@example.com")},
                "organization": {"uuid": format!("org-{who}"), "name": who},
            })))
            .expect(1)
    };
    token("work-code", "access-work-1", "refresh-work-1", 600, "work")
        .mount(&llm)
        .await;
    token(
        "personal-code",
        "access-personal-1",
        "refresh-personal-1",
        3600,
        "personal",
    )
    .mount(&llm)
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_partial_json(
            json!({"grant_type": "refresh_token", "refresh_token": "refresh-work-1"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-work-2", "refresh_token": "refresh-work-2", "expires_in": 3600,
        })))
        .expect(1)
        .mount(&llm)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-test", "display_name": "Claude Test"}], "has_more": false,
        })))
        .mount(&llm)
        .await;
    for (access, text) in [
        ("access-work-2", "Hi from work"),
        ("access-personal-1", "Hi from personal"),
    ] {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", format!("Bearer {access}").as_str()))
            .respond_with(answer(text))
            .mount(&llm)
            .await;
    }
    let env = start(&root).await;
    env.oracle.set_model_auth_base_url("anthropic", &llm.uri());
    let (device, url, laptop) = (&env.device, &env.url, &env.laptop);

    let work = sign_in(device, "work", "work-code").await;
    assert_eq!(work.email.as_deref(), Some("work@example.com"));
    assert_eq!(work.organization.as_deref(), Some("org-work"));
    sign_in(device, "personal", "personal-code").await;
    device.set_secret("npm-work", "npm-token-1").await.unwrap();
    let preset = |account: &str| {
        format!(
            "llm:\n  default_model: anthropic/claude-test\n  accounts:\n    anthropic: {account}\n  providers:\n    anthropic:\n      base_url: {}\nenvironment:\n  secrets:\n    NPM_TOKEN: npm-work\n",
            llm.uri()
        )
    };
    let skill = PresetFile {
        path: "skills/review/SKILL.md".into(),
        content: "---\nname: review\ndescription: Review the work.\n---\nReview it.\n".into(),
    };
    device
        .set_full_preset(
            "work",
            &preset("work"),
            Some("PRESET RULE"),
            Some(vec![skill]),
        )
        .await
        .unwrap();
    device
        .set_full_preset("personal", &preset("personal"), None, None)
        .await
        .unwrap();
    let models = device.models("work").await.unwrap();
    assert!(models.errors.is_empty(), "{:?}", models.errors);
    assert_eq!(
        models.default_model.as_deref(),
        Some("anthropic/claude-test")
    );
    assert!(
        models
            .models
            .iter()
            .any(|m| m.id == "anthropic/claude-test" && m.account.as_deref() == Some("work"))
    );

    device
        .create_workspace("acme", "box", vec![repository(format!("{files}/app.git"))])
        .await
        .unwrap();
    device
        .set_workspace_notes("acme", "NOTE FROM THE ORACLE")
        .await
        .unwrap();
    let first = |preset: &str, text: &str| NewCodespace {
        workspace: "acme".into(),
        preset: Some(preset.into()),
        cpus: Some(1),
        memory_mib: Some(512),
        first_message: Some(FirstMessage {
            text: text.into(),
            model: None,
        }),
        ..Default::default()
    };
    let created = device
        .create_codespace(first("work", "Hello from work"))
        .await
        .unwrap();
    assert!(created.first_message);
    let ready = wait_codespace(device, &created.id, |c| c.state == CodespaceState::Ready).await;
    let request = model_request(&llm, "Hello from work").await;
    assert_eq!(bearer(&request), "Bearer access-work-2");
    let body = String::from_utf8_lossy(&request.body).into_owned();
    assert!(body.contains("PRESET RULE"), "{body}");
    assert!(body.contains("review: Review the work."), "{body}");
    assert!(body.contains("`NPM_TOKEN`"), "{body}");
    assert!(!body.contains("npm-token-1"), "{body}");
    let taken = wait_codespace(device, &created.id, |c| !c.first_message).await;
    assert_eq!(taken.state, CodespaceState::Ready);

    let summaries = wait_for("the oracle has no completed session", async || {
        let sessions = device.sessions(Some(&created.id)).await.unwrap();
        sessions
            .iter()
            .any(|s| s.session.state == "completed")
            .then_some(sessions)
    })
    .await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].workspace, "acme");

    let home = env.codespaces.join(&created.instance).join("home");
    let env_file = home.join(".local/share/loom/secrets.env");
    assert_eq!(
        std::fs::read_to_string(&env_file).unwrap(),
        "NPM_TOKEN='npm-token-1'\n"
    );
    assert!(home.join(".config/loom/skills/review/SKILL.md").is_file());

    let agent = connect_agent(url, &ready, laptop).await.unwrap();
    let sessions = agent.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].policy_default, PolicyAction::Allow);
    let providers = agent.list_providers().await.unwrap();
    let anthropic = providers
        .iter()
        .find(|p| p.provider == "anthropic")
        .unwrap();
    assert!(anthropic.login_methods.is_empty());
    assert_eq!(
        anthropic.accounts[0].label.as_deref(),
        Some("work (work@example.com)")
    );
    assert!(
        agent
            .start_login("anthropic", LoginMethod::DeviceCode)
            .await
            .is_err()
    );
    let notes = agent.notes().await.unwrap();
    assert_eq!(notes.text, "NOTE FROM THE ORACLE");
    agent
        .write_notes("NOTE FROM THE AGENT".into(), notes.version)
        .await
        .unwrap();
    wait_for("the oracle did not get the notes", async || {
        let notes = device.workspace_notes("acme").await.unwrap();
        (notes.text == "NOTE FROM THE AGENT").then_some(())
    })
    .await;

    device.set_secret("npm-work", "npm-token-2").await.unwrap();
    wait_for("the agent did not write the new secret", async || {
        std::fs::read_to_string(&env_file)
            .unwrap()
            .contains("npm-token-2")
            .then_some(())
    })
    .await;

    let other = device
        .create_codespace(first("personal", "Hello from personal"))
        .await
        .unwrap();
    wait_codespace(device, &other.id, |c| c.state == CodespaceState::Ready).await;
    let request = model_request(&llm, "Hello from personal").await;
    assert_eq!(bearer(&request), "Bearer access-personal-1");
    let home = env.codespaces.join(&other.instance).join("home");
    assert!(!home.join(".config/loom/AGENTS.md").exists());

    device
        .set_full_preset("work", &preset("personal"), None, None)
        .await
        .unwrap();
    wait_for("the agent did not change its account", async || {
        let providers = agent.list_providers().await.unwrap();
        providers
            .iter()
            .find(|p| p.provider == "anthropic")
            .and_then(|p| p.accounts.first())
            .and_then(|a| a.label.as_deref())
            .is_some_and(|label| label.starts_with("personal"))
            .then_some(())
    })
    .await;
    agent
        .send_text(&sessions[0].id, "Again with personal")
        .await
        .unwrap();
    let request = model_request(&llm, "Again with personal").await;
    assert_eq!(bearer(&request), "Bearer access-personal-1");

    for id in [&created.id, &other.id] {
        device.close_codespace(id).await.unwrap();
        wait_codespace(device, id, |c| c.state == CodespaceState::Closed).await;
    }
    env.shutdown.cancel();
}
