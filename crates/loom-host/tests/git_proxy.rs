use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use loom_client::oracle_proto::{
    ErrorCode, GitProxyAccess, GitProxyRequest, GitToken, OracleError, Secret,
};
use loom_host::git_proxy::{GitProxy, ProxyBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const UPSTREAM_USER: &str = "x-access-token";
const UPSTREAM_TOKEN: &str = "ghs_real";

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

async fn cgi(root: &Path, head: &str, body: Vec<u8>) -> Vec<u8> {
    let mut first = head.lines().next().unwrap_or_default().split_whitespace();
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
        .env("REMOTE_USER", UPSTREAM_USER)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    if let Some(encoding) = header("content-encoding") {
        command.env("HTTP_CONTENT_ENCODING", encoding);
    }
    if let Some(protocol) = header("git-protocol") {
        command.env("GIT_PROTOCOL", protocol);
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

async fn serve_git(root: PathBuf) -> String {
    let expected = format!(
        "authorization: basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("{UPSTREAM_USER}:{UPSTREAM_TOKEN}"))
    )
    .to_lowercase();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let root = root.clone();
            let expected = expected.clone();
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut stream).await else {
                    return;
                };
                let allowed = head
                    .lines()
                    .any(|line| line.trim().to_lowercase() == expected);
                let response = if allowed {
                    cgi(&root, &head, body).await
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

async fn git(home: &Path, dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .await
        .unwrap()
}

async fn git_ok(home: &Path, dir: &Path, args: &[&str]) {
    let output = git(home, dir, args).await;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Backend {
    upstream: String,
    requests: Mutex<Vec<GitProxyRequest>>,
}

#[async_trait]
impl ProxyBackend for Backend {
    async fn access(&self, request: GitProxyRequest) -> Result<GitProxyAccess, OracleError> {
        self.requests.lock().unwrap().push(request.clone());
        let valid = [("c1", "s3"), ("c2", "s3")];
        if !valid.contains(&(request.codespace.as_str(), request.secret.expose())) {
            return Err(OracleError::new(ErrorCode::Forbidden, "not valid"));
        }
        Ok(GitProxyAccess {
            upstream: format!(
                "{}/{}/{}.git",
                self.upstream, request.owner, request.repository
            ),
            token: Some(GitToken {
                username: UPSTREAM_USER.into(),
                password: Secret::new(UPSTREAM_TOKEN),
                expires_at: None,
            }),
        })
    }

    async fn addresses(&self, codespace: &str) -> anyhow::Result<Vec<IpAddr>> {
        Ok(match codespace {
            "c1" => vec!["127.0.0.1".parse()?],
            "c2" => vec!["10.9.9.9".parse()?],
            _ => Vec::new(),
        })
    }
}

#[tokio::test]
async fn git_goes_through_the_proxy_without_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let served = root.join("served");
    let origin = root.join("origin");
    for path in [&home, &served, &origin] {
        std::fs::create_dir_all(path).unwrap();
    }
    git_ok(&home, &origin, &["init", "--quiet", "-b", "main"]).await;
    std::fs::write(origin.join("README.md"), "hello\n").unwrap();
    git_ok(&home, &origin, &["add", "README.md"]).await;
    git_ok(&home, &origin, &["commit", "--quiet", "-m", "first"]).await;
    std::fs::create_dir_all(served.join("acme")).unwrap();
    let bare = served.join("acme/server.git");
    git_ok(
        &home,
        &root,
        &[
            "clone",
            "--quiet",
            "--bare",
            origin.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    )
    .await;
    git_ok(&home, &bare, &["config", "http.receivepack", "true"]).await;

    let upstream = serve_git(served).await;
    let backend = Arc::new(Backend {
        upstream,
        requests: Mutex::new(Vec::new()),
    });
    let proxy = Arc::new(GitProxy::new(backend.clone()).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    tokio::spawn(proxy.serve(listener, shutdown.clone()));

    let helper = |user: &str, password: &str| {
        format!("credential.helper=!f() {{ echo username={user}; echo password={password}; }}; f")
    };
    let url = format!("{proxy_url}/github/acme/server.git");
    let good = helper("c1", "s3");
    git_ok(
        &home,
        &root,
        &["-c", &good, "clone", "--quiet", &url, "clone"],
    )
    .await;
    let clone = root.join("clone");
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).unwrap(),
        "hello\n"
    );
    std::fs::write(clone.join("NEW.md"), "new\n").unwrap();
    git_ok(&home, &clone, &["add", "NEW.md"]).await;
    git_ok(&home, &clone, &["commit", "--quiet", "-m", "second"]).await;
    git_ok(
        &home,
        &clone,
        &["-c", &good, "push", "--quiet", "origin", "main"],
    )
    .await;
    let log = git(&home, &bare, &["log", "--format=%s", "main"]).await;
    assert_eq!(String::from_utf8_lossy(&log.stdout), "second\nfirst\n");
    let config = std::fs::read_to_string(clone.join(".git/config")).unwrap();
    assert!(!config.contains(UPSTREAM_TOKEN));

    let bad = helper("c1", "wrong");
    let output = git(
        &home,
        &root,
        &["-c", &bad, "clone", "--quiet", &url, "refused"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("403"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let elsewhere = helper("c2", "s3");
    let output = git(
        &home,
        &root,
        &["-c", &elsewhere, "clone", "--quiet", &url, "elsewhere"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        !backend
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.codespace == "c2"),
        "the proxy asked the oracle for a codespace at a different address"
    );

    let output = git(
        &home,
        &root,
        &[
            "-c",
            &good,
            "clone",
            "--quiet",
            &format!("{proxy_url}/github/acme/server.git/../../x"),
            "odd",
        ],
    )
    .await;
    assert!(
        !output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = backend.requests.lock().unwrap();
    assert!(requests.iter().all(|r| r.provider == "github"));
    shutdown.cancel();
}
