use std::io::{BufRead, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use loom_client::OracleClient;
use loom_client::oracle_proto::{GitProviderSetup, GitSetup};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

const GIT_DIR: &str = "git";
const INCLUDE_FILE: &str = "config";
const SOCKET_FILE: &str = "credential.sock";
const GH_STATE_FILE: &str = "gh-hosts.json";
const GH_HOSTS_FILE: &str = "hosts.yml";
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LINE: u64 = 8192;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperRequest {
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedClone {
    pub path: PathBuf,
    pub provider: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GitPaths {
    pub dir: PathBuf,
    pub gh_dir: PathBuf,
    pub agent: PathBuf,
}

impl GitPaths {
    pub fn new(data_dir: &Path, config_home: &Path) -> anyhow::Result<Self> {
        let gh_dir = std::env::var_os("GH_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_home.join("gh"));
        Ok(Self {
            dir: data_dir.join(GIT_DIR),
            gh_dir,
            agent: std::env::current_exe().context("cannot find the path of loom-agent")?,
        })
    }

    pub fn include(&self) -> PathBuf {
        self.dir.join(INCLUDE_FILE)
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join(SOCKET_FILE)
    }

    fn identity(&self, provider: &str) -> PathBuf {
        self.dir.join(format!("identity-{provider}"))
    }
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("cannot make {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot protect {}", path.display()))
}

fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    let partial = path.with_extension("partial");
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&partial, path)
    };
    write().with_context(|| format!("cannot write {}", path.display()))
}

fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn shell_word(value: &Path) -> anyhow::Result<String> {
    let text = value.to_str().context("a path is not UTF-8")?;
    if text.contains('\'') {
        bail!("the path {text:?} has a quote");
    }
    Ok(format!("'{text}'"))
}

fn valid_value(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

pub fn include_text(paths: &GitPaths, setup: &GitSetup) -> anyhow::Result<String> {
    let helper = format!(
        "!{} git-credential --socket {}",
        shell_word(&paths.agent)?,
        shell_word(&paths.socket())?
    );
    let mut text = String::new();
    for provider in &setup.providers {
        let url = quoted(&provider.web_url);
        if let Some(proxy) = &provider.proxy_url {
            text.push_str(&format!(
                "[url {}]\n\tinsteadOf = {}\n",
                quoted(&format!("{proxy}/")),
                quoted(&format!("{}/", provider.web_url))
            ));
            text.push_str(&format!(
                "[credential {}]\n\thelper =\n\thelper = {}\n\tuseHttpPath = true\n",
                quoted(proxy),
                quoted(&helper)
            ));
        } else if provider.credentials {
            text.push_str(&format!(
                "[credential {url}]\n\thelper =\n\thelper = {}\n\tuseHttpPath = true\n",
                quoted(&helper)
            ));
        }
        if provider.git_name.is_some() || provider.git_email.is_some() {
            let condition = quoted(&format!("hasconfig:remote.*.url:{}/**", provider.web_url));
            let file = paths.identity(&provider.name);
            let file = file.to_str().context("a path is not UTF-8")?;
            text.push_str(&format!(
                "[includeIf {condition}]\n\tpath = {}\n",
                quoted(file)
            ));
        }
    }
    Ok(text)
}

fn identity_text(provider: &GitProviderSetup) -> String {
    let mut text = String::from("[user]\n");
    if let Some(name) = provider.git_name.as_deref().filter(|n| valid_value(n)) {
        text.push_str(&format!("\tname = {}\n", quoted(name)));
    }
    if let Some(email) = provider.git_email.as_deref().filter(|e| valid_value(e)) {
        text.push_str(&format!("\temail = {}\n", quoted(email)));
    }
    text
}

pub fn gh_host(web_url: &str) -> Option<String> {
    let rest = web_url.split_once("://")?.1;
    let host = rest.split('/').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn gh_entry(user: &str, token: &str) -> Value {
    json!({
        "users": {user: {"oauth_token": token}},
        "git_protocol": "https",
        "user": user,
        "oauth_token": token,
    })
}

pub fn merge_gh_hosts(
    existing: Map<String, Value>,
    managed_before: &[String],
    setup: &GitSetup,
) -> (Map<String, Value>, Vec<String>) {
    let mut hosts = existing;
    for host in managed_before {
        hosts.remove(host);
    }
    let mut managed = Vec::new();
    for provider in &setup.providers {
        let (Some(gh), Some(host)) = (&provider.gh, gh_host(&provider.web_url)) else {
            continue;
        };
        hosts.insert(host.clone(), gh_entry(&gh.user, gh.token.expose()));
        managed.push(host);
    }
    (hosts, managed)
}

fn write_gh(paths: &GitPaths, setup: &GitSetup) -> anyhow::Result<()> {
    let state_path = paths.dir.join(GH_STATE_FILE);
    let before: Vec<String> = std::fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let hosts_path = paths.gh_dir.join(GH_HOSTS_FILE);
    let existing = match std::fs::read_to_string(&hosts_path) {
        Ok(text) if !text.trim().is_empty() => {
            match serde_saphyr::from_str::<Map<String, Value>>(&text) {
                Ok(map) => map,
                Err(err) => {
                    tracing::warn!(error = %err, path = %hosts_path.display(), "cannot read the hosts of gh; writing a new file");
                    Map::new()
                }
            }
        }
        _ => Map::new(),
    };
    if before.is_empty() && setup.providers.iter().all(|p| p.gh.is_none()) {
        return Ok(());
    }
    let (hosts, managed) = merge_gh_hosts(existing, &before, setup);
    private_dir(&paths.gh_dir)?;
    let text = if hosts.is_empty() {
        String::new()
    } else {
        serde_saphyr::to_string(&hosts).context("cannot write the hosts of gh")?
    };
    write_private(&hosts_path, &text)?;
    write_private(&state_path, &serde_json::to_string(&managed)?)
}

async fn git_output(args: &[&str], dir: Option<&Path>) -> anyhow::Result<std::process::Output> {
    git_output_within(args, dir, GIT_TIMEOUT).await
}

pub(crate) async fn git_output_within(
    args: &[&str],
    dir: Option<&Path>,
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| anyhow::anyhow!("git {} took too long", args.join(" ")))?
        .context("cannot run git")
}

async fn git_ok(args: &[&str], dir: Option<&Path>) -> anyhow::Result<()> {
    let output = git_output(args, dir).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

async fn include_globally(include: &Path) -> anyhow::Result<()> {
    let include = include.to_str().context("a path is not UTF-8")?;
    let output = git_output(&["config", "--global", "--get-all", "include.path"], None).await?;
    let present = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line == include);
    if !present {
        git_ok(
            &["config", "--global", "--add", "include.path", include],
            None,
        )
        .await?;
    }
    Ok(())
}

async fn set_local(clone: &Path, key: &str, value: Option<&str>) -> anyhow::Result<()> {
    match value {
        Some(value) => git_ok(&["config", "--local", key, value], Some(clone)).await,
        None => {
            let output =
                git_output(&["config", "--local", "--unset-all", key], Some(clone)).await?;
            match output.status.code() {
                Some(0 | 5) => Ok(()),
                _ => bail!(
                    "git config --unset-all {key} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            }
        }
    }
}

pub async fn set_identity(clone: &ManagedClone, setup: &GitSetup) -> anyhow::Result<()> {
    let provider = clone
        .provider
        .as_ref()
        .and_then(|name| setup.providers.iter().find(|p| &p.name == name));
    let name = provider
        .and_then(|p| p.git_name.as_deref())
        .filter(|v| valid_value(v));
    let email = provider
        .and_then(|p| p.git_email.as_deref())
        .filter(|v| valid_value(v));
    set_local(&clone.path, "user.name", name).await?;
    set_local(&clone.path, "user.email", email).await
}

pub async fn apply(paths: &GitPaths, setup: &GitSetup) -> anyhow::Result<()> {
    private_dir(&paths.dir)?;
    for provider in &setup.providers {
        if provider.git_name.is_some() || provider.git_email.is_some() {
            write_private(&paths.identity(&provider.name), &identity_text(provider))?;
        }
    }
    write_private(&paths.include(), &include_text(paths, setup)?)?;
    include_globally(&paths.include()).await?;
    write_gh(paths, setup)
}

pub async fn apply_all(
    paths: &GitPaths,
    setup: &GitSetup,
    clones: &[ManagedClone],
) -> anyhow::Result<()> {
    apply(paths, setup).await?;
    for clone in clones.iter().filter(|c| c.path.join(".git").exists()) {
        set_identity(clone, setup)
            .await
            .with_context(|| format!("cannot set the Git identity of {}", clone.path.display()))?;
    }
    Ok(())
}

async fn answer(stream: UnixStream, client: &OracleClient) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    let request: HelperRequest = serde_json::from_str(&line).context("a malformed request")?;
    let reply =
        match tokio::time::timeout(HELPER_TIMEOUT, client.git_credential(&request.url)).await {
            Ok(Ok(result)) => match result.token {
                Some(token) => HelperReply {
                    username: Some(token.username),
                    password: Some(token.password.expose().to_owned()),
                    expires_at: token.expires_at,
                    error: None,
                },
                None => HelperReply::default(),
            },
            Ok(Err(err)) => HelperReply {
                error: Some(err.to_string()),
                ..HelperReply::default()
            },
            Err(_) => HelperReply {
                error: Some("the oracle did not answer".to_owned()),
                ..HelperReply::default()
            },
        };
    let mut bytes = serde_json::to_vec(&reply)?;
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.shutdown().await?;
    Ok(())
}

pub fn serve_credentials(
    paths: &GitPaths,
    client: watch::Receiver<OracleClient>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    private_dir(&paths.dir)?;
    let socket = paths.socket();
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("cannot remove {}", socket.display())),
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("cannot listen on {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot protect {}", socket.display()))?;
    Ok(tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(err) => {
                    tracing::warn!(error = %err, "cannot accept a Git credential request");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let client = client.borrow().clone();
            tokio::spawn(async move {
                if let Err(err) = answer(stream, &client).await {
                    tracing::warn!(error = %format!("{err:#}"), "cannot answer a Git credential request");
                }
            });
        }
    }))
}

pub fn request_url(input: &str) -> Option<String> {
    let mut protocol = None;
    let mut host = None;
    let mut path = None;
    for line in input.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "protocol" => protocol = Some(value),
            "host" => host = Some(value),
            "path" => path = Some(value),
            "url" => return Some(value.to_owned()),
            _ => {}
        }
    }
    let (protocol, host) = (protocol?, host?);
    Some(match path {
        Some(path) => format!("{protocol}://{host}/{}", path.trim_start_matches('/')),
        None => format!("{protocol}://{host}"),
    })
}

pub fn helper_output(reply: &HelperReply) -> String {
    let mut text = String::new();
    if let (Some(username), Some(password)) = (&reply.username, &reply.password) {
        text.push_str(&format!("username={username}\npassword={password}\n"));
        if let Some(expires_at) = reply.expires_at {
            text.push_str(&format!("password_expiry_utc={expires_at}\n"));
        }
    }
    text
}

pub async fn credential_helper(socket: &Path, operation: &str) -> anyhow::Result<()> {
    if operation != "get" {
        return Ok(());
    }
    let mut input = String::new();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.is_empty() {
            break;
        }
        input.push_str(&line);
        input.push('\n');
    }
    let Some(url) = request_url(&input) else {
        return Ok(());
    };
    let mut stream = tokio::time::timeout(HELPER_TIMEOUT, UnixStream::connect(socket))
        .await
        .context("the codespace agent did not answer")?
        .with_context(|| format!("cannot reach the codespace agent at {}", socket.display()))?;
    let mut request = serde_json::to_vec(&HelperRequest { url })?;
    request.push(b'\n');
    stream.write_all(&request).await?;
    let mut line = String::new();
    tokio::time::timeout(
        HELPER_TIMEOUT,
        BufReader::new(stream.take(MAX_LINE)).read_line(&mut line),
    )
    .await
    .context("the codespace agent did not answer")??;
    let reply: HelperReply = serde_json::from_str(&line).context("a malformed answer")?;
    if let Some(error) = &reply.error {
        bail!("the oracle did not give a token: {error}");
    }
    let mut stdout = std::io::stdout();
    stdout.write_all(helper_output(&reply).as_bytes())?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use loom_client::oracle_proto::{GhLogin, Secret};

    use super::*;

    fn provider(name: &str, web_url: &str, credentials: bool) -> GitProviderSetup {
        GitProviderSetup {
            name: name.into(),
            web_url: web_url.into(),
            api_url: format!("{web_url}/api/v3"),
            git_name: None,
            git_email: None,
            credentials,
            gh: None,
            proxy_url: None,
        }
    }

    fn paths(dir: &Path) -> GitPaths {
        GitPaths {
            dir: dir.join("git"),
            gh_dir: dir.join("gh"),
            agent: PathBuf::from("/usr/bin/loom-agent"),
        }
    }

    #[test]
    fn git_reads_the_helper_and_the_identity_of_each_provider() {
        let dir = Path::new("/home/dev/.local/share/loom");
        let mut ghe = provider("ghe", "https://ghe.example.com", true);
        ghe.git_name = Some("Dev \"D\"".into());
        let setup = GitSetup {
            providers: vec![ghe, provider("github", "https://github.com", false)],
        };
        let text = include_text(&paths(dir), &setup).unwrap();
        assert_eq!(
            text,
            "[credential \"https://ghe.example.com\"]\n\thelper =\n\thelper = \"!'/usr/bin/loom-agent' git-credential --socket '/home/dev/.local/share/loom/git/credential.sock'\"\n\tuseHttpPath = true\n[includeIf \"hasconfig:remote.*.url:https://ghe.example.com/**\"]\n\tpath = \"/home/dev/.local/share/loom/git/identity-ghe\"\n"
        );
        assert_eq!(
            identity_text(&setup.providers[0]),
            "[user]\n\tname = \"Dev \\\"D\\\"\"\n"
        );
        let mut proxied = provider("github", "https://github.com", true);
        proxied.proxy_url = Some("http://10.0.0.1:7440/github".into());
        let setup = GitSetup {
            providers: vec![proxied],
        };
        let text = include_text(&paths(dir), &setup).unwrap();
        assert_eq!(
            text,
            "[url \"http://10.0.0.1:7440/github/\"]\n\tinsteadOf = \"https://github.com/\"\n[credential \"http://10.0.0.1:7440/github\"]\n\thelper =\n\thelper = \"!'/usr/bin/loom-agent' git-credential --socket '/home/dev/.local/share/loom/git/credential.sock'\"\n\tuseHttpPath = true\n"
        );
    }

    #[test]
    fn gh_keeps_the_hosts_that_loom_does_not_manage() {
        let mut ghe = provider("ghe", "https://GHE.example.com:8443", true);
        ghe.gh = Some(GhLogin {
            user: "octo".into(),
            token: Secret::new("t1"),
        });
        let setup = GitSetup {
            providers: vec![ghe, provider("github", "https://github.com", false)],
        };
        let mut existing = Map::new();
        existing.insert("mine.example.com".into(), json!({"user": "me"}));
        existing.insert("old.example.com".into(), json!({"user": "loom"}));
        let (hosts, managed) = merge_gh_hosts(existing, &["old.example.com".to_owned()], &setup);
        assert_eq!(managed, vec!["ghe.example.com:8443".to_owned()]);
        assert!(hosts.contains_key("mine.example.com"));
        assert!(!hosts.contains_key("old.example.com"));
        assert_eq!(
            hosts["ghe.example.com:8443"]["users"]["octo"]["oauth_token"],
            "t1"
        );
        assert_eq!(hosts["ghe.example.com:8443"]["git_protocol"], "https");
    }

    #[test]
    fn the_hosts_file_of_gh_is_yaml_that_survives_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(&paths.gh_dir).unwrap();
        std::fs::write(
            paths.gh_dir.join(GH_HOSTS_FILE),
            "mine.example.com:\n    user: me\n    oauth_token: x\n",
        )
        .unwrap();
        let mut github = provider("github", "https://github.com", true);
        github.gh = Some(GhLogin {
            user: "octo".into(),
            token: Secret::new("t2"),
        });
        let setup = GitSetup {
            providers: vec![github],
        };
        private_dir(&paths.dir).unwrap();
        write_gh(&paths, &setup).unwrap();
        let text = std::fs::read_to_string(paths.gh_dir.join(GH_HOSTS_FILE)).unwrap();
        let hosts: Map<String, Value> = serde_saphyr::from_str(&text).unwrap();
        assert_eq!(hosts["mine.example.com"]["oauth_token"], "x");
        assert_eq!(hosts["github.com"]["oauth_token"], "t2");
        let mode = std::fs::metadata(paths.gh_dir.join(GH_HOSTS_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        write_gh(&paths, &GitSetup::default()).unwrap();
        let text = std::fs::read_to_string(paths.gh_dir.join(GH_HOSTS_FILE)).unwrap();
        let hosts: Map<String, Value> = serde_saphyr::from_str(&text).unwrap();
        assert!(!hosts.contains_key("github.com"));
        assert!(hosts.contains_key("mine.example.com"));
    }

    #[test]
    fn the_helper_reads_the_request_of_git() {
        assert_eq!(
            request_url("protocol=https\nhost=ghe.example.com:8443\npath=acme/app.git\n"),
            Some("https://ghe.example.com:8443/acme/app.git".to_owned())
        );
        assert_eq!(
            request_url("protocol=https\nhost=github.com\n"),
            Some("https://github.com".to_owned())
        );
        assert_eq!(request_url("host=github.com\n"), None);
        let reply = HelperReply {
            username: Some("octo".into()),
            password: Some("t".into()),
            expires_at: Some(99),
            error: None,
        };
        assert_eq!(
            helper_output(&reply),
            "username=octo\npassword=t\npassword_expiry_utc=99\n"
        );
        assert_eq!(helper_output(&HelperReply::default()), "");
    }
}
