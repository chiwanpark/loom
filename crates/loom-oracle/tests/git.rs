use std::sync::Arc;
use std::time::Duration;

use loom_client::oracle_proto::{
    AccountKind, CheckResult, CodespaceApiRequest, CodespaceState, ErrorCode, Event, FirstMessage,
    GitCredentialInput, GitProviderInput, GitProxyRequest, HostCall, HostCheck, HostCheckStatus,
    HostReport, ModelLoginMethod, OracleError, PresetFile, ProviderRoute, Repository, Role, Secret,
    SessionSummary, VolumeInput, VolumeKind, WorkspaceCheckResult, WorkspaceVolume,
};
use loom_client::{
    ClientError, ClientIdentity, NewCodespace, OracleCalls, OracleClient, OracleEvents, pair_oracle,
};
use loom_config::{Dirs, OracleConfig};
use loom_github::{ApiRequest, Http, Method};
use loom_oracle::Oracle;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WAIT: Duration = Duration::from_secs(10);

struct TestOracle {
    _dir: tempfile::TempDir,
    oracle: Arc<Oracle>,
    _shutdown: CancellationToken,
}

async fn start_oracle() -> TestOracle {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = OracleConfig {
        url: Some(format!("ws://{addr}")),
        listen: addr.to_string(),
        ..Default::default()
    };
    let home = dir.path().join("home");
    let dirs = Dirs {
        config_home: home.join(".config"),
        data_home: home.join(".local/share"),
        state_home: home.join(".local/state"),
        runtime_dir: None,
        home,
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
        _shutdown: shutdown,
    }
}

impl TestOracle {
    async fn connect(&self, role: Role, label: &str) -> (OracleClient, OracleEvents) {
        let link = self
            .oracle
            .create_pairing(role, Duration::from_secs(60), None)
            .unwrap()
            .link;
        let identity = ClientIdentity::generate().unwrap();
        let endpoint = pair_oracle(&link, &identity, label, None).await.unwrap();
        OracleClient::connect(&endpoint, &identity, "test")
            .await
            .unwrap()
    }
}

fn code(err: ClientError) -> ErrorCode {
    match err {
        ClientError::Oracle(err) => err.code,
        other => panic!("expected an oracle error, got {other}"),
    }
}

fn message(err: ClientError) -> String {
    match err {
        ClientError::Oracle(err) => err.message,
        other => panic!("expected an oracle error, got {other}"),
    }
}

async fn next_event(events: &mut OracleEvents, check: impl Fn(&Event) -> bool) -> Event {
    loop {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("the events ended");
        if check(&event) {
            return event;
        }
    }
}

async fn connect_host(oracle: &TestOracle, label: &str) -> (OracleClient, OracleCalls) {
    let link = oracle
        .oracle
        .create_pairing(Role::Host, Duration::from_secs(60), None)
        .unwrap()
        .link;
    let identity = ClientIdentity::generate().unwrap();
    let endpoint = pair_oracle(&link, &identity, label, None).await.unwrap();
    let (host, _events, calls) =
        OracleClient::connect_with_calls(&endpoint, &identity, "test-host")
            .await
            .unwrap();
    host.host_report(HostReport {
        version: "test".into(),
        driver: "process".into(),
        cpus: 4,
        memory_mib: 8192,
        ..Default::default()
    })
    .await
    .unwrap();
    (host, calls)
}

async fn start_host(oracle: &TestOracle) -> (OracleClient, mpsc::UnboundedReceiver<HostCall>) {
    let (host, mut calls) = connect_host(oracle, "box").await;
    let (tx, rx) = mpsc::unbounded_channel();
    let client = host.clone();
    tokio::spawn(async move {
        let http = Http::new().unwrap();
        while let Some((id, call)) = calls.recv().await {
            let result = match &call {
                HostCall::CheckWorkspace(check) => {
                    let mut result = WorkspaceCheckResult::default();
                    for provider in &check.providers {
                        result.providers.push(CheckResult {
                            name: provider.name.clone(),
                            error: http.reach(&provider.api_url).await.err(),
                        });
                    }
                    Ok(serde_json::to_value(result).unwrap())
                }
                HostCall::ProviderRequest(request) => {
                    let api = ApiRequest {
                        method: Method::parse(&request.method).unwrap(),
                        path: request.path.clone(),
                        query: request.query.clone(),
                        body: request.body.clone(),
                    };
                    let token = request.token.as_ref().map(Secret::expose);
                    let response = http.send(&request.api_url, token, &api).await.unwrap();
                    Ok(serde_json::to_value(response).unwrap())
                }
                _ => {
                    let _ = tx.send(call.clone());
                    continue;
                }
            };
            client.reply(id, result).await.unwrap();
        }
    });
    (host, rx)
}

fn repo(owner: &str, name: &str, web: &str) -> serde_json::Value {
    json!({
        "name": name,
        "owner": {"login": owner},
        "private": true,
        "default_branch": "main",
        "html_url": format!("{web}/{owner}/{name}"),
        "clone_url": format!("{web}/{owner}/{name}.git"),
    })
}

async fn github(users: &[(&str, &str, u64)]) -> MockServer {
    let server = MockServer::start().await;
    for (token, login, id) in users {
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", format!("Bearer {token}").as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "github-authentication-token-expiration",
                        "2030-01-01 00:00:00 UTC",
                    )
                    .set_body_json(json!({"login": login, "id": id})),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        )
        .mount(&server)
        .await;
    server
}

fn repository(url: &str) -> Repository {
    Repository {
        url: url.into(),
        provider: None,
        directory: None,
        base_branch: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn providers_credentials_and_presets_set_up_the_git_of_a_codespace() {
    let oracle = start_oracle().await;
    let (laptop, mut events) = oracle.connect(Role::Device, "laptop").await;
    laptop.subscribe().await.unwrap();

    let hub = github(&[("t-work", "octo", 7), ("t-all", "octo-all", 8)]).await;
    let web = hub.uri();
    let provider = laptop
        .set_git_provider(GitProviderInput {
            name: "github".into(),
            web_url: format!("{web}/"),
            api_url: Some(web.clone()),
            git_name: None,
            git_email: None,
            route: ProviderRoute::Direct,
        })
        .await
        .unwrap();
    assert_eq!(provider.web_url, web);
    assert_eq!(
        code(
            laptop
                .set_git_provider(GitProviderInput {
                    name: "copy".into(),
                    web_url: web.clone(),
                    api_url: None,
                    git_name: None,
                    git_email: None,
                    route: ProviderRoute::Direct,
                })
                .await
                .unwrap_err()
        ),
        ErrorCode::Conflict
    );

    let credential = |name: &str, owner: Option<&str>, token: Option<&str>| GitCredentialInput {
        name: name.into(),
        provider: "github".into(),
        owner: owner.map(str::to_owned),
        repository: None,
        token: token.map(Secret::new),
    };
    let work = laptop
        .set_git_credential(credential("work", Some("acme"), Some("t-work")))
        .await
        .unwrap();
    assert_eq!(work.login.as_deref(), Some("octo"));
    assert!(work.expires_at.is_some());
    assert_eq!(work.scope(), "acme/*");
    laptop
        .set_git_credential(credential("all", None, Some("t-all")))
        .await
        .unwrap();
    let broken = laptop
        .set_git_credential(credential("broken", None, Some("nope")))
        .await
        .unwrap();
    assert_eq!(broken.login, None);
    assert!(broken.check_error.unwrap().contains("Bad credentials"));
    assert_eq!(
        code(
            laptop
                .set_git_credential(credential("fresh", None, None))
                .await
                .unwrap_err()
        ),
        ErrorCode::Conflict
    );
    let listed = laptop.git_credentials().await.unwrap();
    assert_eq!(listed.len(), 3);
    assert!(!serde_json::to_string(&listed).unwrap().contains("t-work"));
    next_event(
        &mut events,
        |e| matches!(e, Event::GitCredentialChanged { credential } if credential.name == "broken"),
    )
    .await;
    laptop.delete_git_credential("broken").await.unwrap();

    assert_eq!(
        code(
            laptop
                .set_preset("dev", "git:\n  credentials: [missing]\n")
                .await
                .unwrap_err()
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(laptop.set_preset("dev", "git: [").await.unwrap_err()),
        ErrorCode::BadRequest
    );
    let preset = laptop
        .set_preset("dev", "git:\n  credentials: [all, work]\n")
        .await
        .unwrap();
    assert_eq!(
        preset.git_credentials,
        vec!["all".to_owned(), "work".to_owned()]
    );

    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            repo("acme", "server", &web),
            repo("me", "notes", &web)
        ])))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/search/repositories"))
        .and(query_param("q", "server in:name"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [repo("acme", "server", &web), repo("other", "server", &web)]
        })))
        .mount(&hub)
        .await;
    let found = laptop
        .search_repositories("github", "server")
        .await
        .unwrap();
    let names: Vec<String> = found
        .repositories
        .iter()
        .map(|r| format!("{}/{}", r.owner, r.name))
        .collect();
    assert_eq!(
        names,
        vec!["acme/server".to_owned(), "other/server".to_owned()]
    );
    assert_eq!(found.repositories[0].url, format!("{web}/acme/server.git"));
    Mock::given(method("GET"))
        .and(path("/repos/acme/server"))
        .and(header("authorization", "Bearer t-work"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo("acme", "server", &web)))
        .mount(&hub)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/server/branches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([{"name": "main"}, {"name": "dev"}])),
        )
        .mount(&hub)
        .await;
    let branches = laptop.branches("github", "acme", "server").await.unwrap();
    assert_eq!(branches.branches, vec!["main".to_owned(), "dev".to_owned()]);
    assert_eq!(branches.default_branch.as_deref(), Some("main"));
    assert_eq!(
        code(laptop.branches("github", "acme", "gone").await.unwrap_err()),
        ErrorCode::NotFound
    );

    let (_host, mut calls) = start_host(&oracle).await;
    let far = laptop
        .set_git_provider(GitProviderInput {
            name: "far".into(),
            web_url: "http://127.0.0.1:1".into(),
            api_url: None,
            git_name: None,
            git_email: None,
            route: ProviderRoute::Direct,
        })
        .await
        .unwrap();
    assert_eq!(far.api_url, "http://127.0.0.1:1/api/v3");

    let ghe = github(&[]).await;
    Mock::given(method("GET"))
        .and(path("/search/repositories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [repo("team", "app", &ghe.uri())]
        })))
        .mount(&ghe)
        .await;
    laptop
        .set_git_provider(GitProviderInput {
            name: "ghe".into(),
            web_url: ghe.uri(),
            api_url: Some(ghe.uri()),
            git_name: Some("Dev".into()),
            git_email: Some("dev@example.com".into()),
            route: ProviderRoute::Hosts,
        })
        .await
        .unwrap();
    let found = laptop.search_repositories("ghe", "app").await.unwrap();
    assert_eq!(found.repositories[0].name, "app");
    assert!(
        ghe.received_requests()
            .await
            .unwrap()
            .iter()
            .any(|r| r.url.path() == "/search/repositories")
    );

    let err = laptop
        .create_workspace("acme", "box", vec![repository("far:acme/server")])
        .await
        .unwrap_err();
    assert!(message(err).contains("cannot reach the Git provider far"));
    let workspace = laptop
        .create_workspace(
            "acme",
            "box",
            vec![
                repository("github:acme/server"),
                repository(&format!("{}/team/app", ghe.uri())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        workspace.repositories[0].url,
        format!("{web}/acme/server.git")
    );
    assert_eq!(
        workspace.repositories[0].provider.as_deref(),
        Some("github")
    );
    assert_eq!(workspace.repositories[1].provider.as_deref(), Some("ghe"));
    assert_eq!(workspace.repositories[1].directory.as_deref(), Some("app"));
    let hosts = laptop.hosts().await.unwrap();
    let reach = &hosts[0].reach;
    assert!(reach.iter().any(|r| r.name == "far" && r.error.is_some()));
    assert!(
        reach
            .iter()
            .any(|r| r.name == "github" && r.error.is_none())
    );
    assert_eq!(
        code(
            laptop
                .update_workspace(
                    "acme",
                    None,
                    Some(vec![
                        repository("github:acme/server"),
                        repository("github:acme/server")
                    ]),
                    None
                )
                .await
                .unwrap_err()
        ),
        ErrorCode::BadRequest
    );

    let warnings = laptop
        .codespace_warnings("acme", Some("dev"))
        .await
        .unwrap();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("no Git credential for team/app on ghe"));
    let warnings = laptop.codespace_warnings("acme", None).await.unwrap();
    assert!(warnings[0].contains("no preset"), "{warnings:?}");
    assert_eq!(
        code(
            laptop
                .codespace_warnings("acme", Some("missing"))
                .await
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    let created = laptop
        .create_codespace_with_warnings(NewCodespace {
            workspace: "acme".into(),
            preset: Some("dev".into()),
            base_branches: [("app".to_owned(), "release".to_owned())].into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(created.codespace.preset.as_deref(), Some("dev"));
    assert_eq!(created.warnings.len(), 2, "{:?}", created.warnings);
    assert!(created.warnings[0].contains("no Git credential for team/app on ghe"));
    assert!(created.warnings[1].contains("github has no Git email"));
    let create = loop {
        match tokio::time::timeout(WAIT, calls.recv())
            .await
            .unwrap()
            .unwrap()
        {
            HostCall::CreateInstance(create) => break create,
            _ => continue,
        }
    };
    let agent_key = ClientIdentity::generate().unwrap();
    let endpoint = pair_oracle(&create.bootstrap.link, &agent_key, "agent", None)
        .await
        .unwrap();
    let (agent, mut agent_events) = OracleClient::connect(&endpoint, &agent_key, "agent")
        .await
        .unwrap();
    agent.subscribe().await.unwrap();
    assert_eq!(
        created.codespace.repositories[1].base_branch.as_deref(),
        Some("release")
    );
    laptop
        .update_workspace(
            "acme",
            None,
            Some(vec![repository("github:acme/server")]),
            None,
        )
        .await
        .unwrap();
    let bundle = agent.bundle().await.unwrap();
    assert_eq!(bundle.preset.as_deref(), Some("dev"));
    assert_eq!(
        bundle.repositories[1].base_branch.as_deref(),
        Some("release")
    );
    assert_eq!(bundle.repositories, created.codespace.repositories);
    let providers = &bundle.git.providers;
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0].name, "github");
    assert!(providers[0].credentials);
    assert_eq!(providers[0].git_name.as_deref(), Some("octo"));
    let gh = providers[0].gh.as_ref().unwrap();
    assert_eq!((gh.user.as_str(), gh.token.expose()), ("octo", "t-work"));
    assert_eq!(providers[1].name, "ghe");
    assert!(!providers[1].credentials);
    assert!(providers[1].gh.is_none());
    assert_eq!(providers[1].git_email.as_deref(), Some("dev@example.com"));

    let token = agent
        .git_credential(&format!("{web}/acme/server.git"))
        .await
        .unwrap()
        .token
        .unwrap();
    assert_eq!(
        (token.username.as_str(), token.password.expose()),
        ("octo", "t-work")
    );
    let token = agent
        .git_credential(&format!("{web}/other/thing"))
        .await
        .unwrap()
        .token
        .unwrap();
    assert_eq!(token.password.expose(), "t-all");
    let none = agent
        .git_credential(&format!("{}/team/app.git", ghe.uri()))
        .await
        .unwrap();
    assert_eq!(none.token, None);
    assert_eq!(
        code(laptop.git_credential("https://x/a/b").await.unwrap_err()),
        ErrorCode::Forbidden
    );

    laptop
        .set_preset("dev", "git:\n  credentials: [work]\n")
        .await
        .unwrap();
    next_event(&mut agent_events, |e| matches!(e, Event::GitChanged)).await;
    let token = agent
        .git_credential(&format!("{web}/other/thing"))
        .await
        .unwrap();
    assert_eq!(token.token, None);

    for (result, conflict) in [
        (laptop.delete_git_provider("github").await, "credentials"),
        (laptop.delete_git_credential("work").await, "presets"),
        (laptop.delete_preset("dev").await, "codespaces"),
    ] {
        let err = result.unwrap_err();
        assert!(message(err).contains(conflict));
    }
    laptop.delete_git_provider("far").await.unwrap();
    assert_eq!(
        code(laptop.delete_git_provider("far").await.unwrap_err()),
        ErrorCode::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_with_a_git_proxy_keeps_the_tokens_out_of_the_codespace() {
    let oracle = start_oracle().await;
    let (laptop, _events) = oracle.connect(Role::Device, "laptop").await;
    let hub = MockServer::start().await;
    let web = hub.uri();
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "me", "id": 1})))
        .mount(&hub)
        .await;
    laptop
        .set_git_provider(GitProviderInput {
            name: "ghe".into(),
            web_url: web.clone(),
            api_url: Some(web.clone()),
            git_name: Some("Dev".into()),
            git_email: Some("dev@example.com".into()),
            route: ProviderRoute::Direct,
        })
        .await
        .unwrap();
    laptop
        .set_git_credential(GitCredentialInput {
            name: "work".into(),
            provider: "ghe".into(),
            owner: Some("acme".into()),
            repository: None,
            token: Some(Secret::new("ghp_real")),
        })
        .await
        .unwrap();
    laptop
        .set_preset("dev", "git:\n  credentials: [work]\n")
        .await
        .unwrap();
    let (host, mut calls) = start_host(&oracle).await;
    let proxy = "http://10.0.0.1:7440";
    host.host_report(HostReport {
        version: "test".into(),
        driver: "incus".into(),
        cpus: 4,
        memory_mib: 8192,
        git_proxy: Some(proxy.into()),
        ..Default::default()
    })
    .await
    .unwrap();
    laptop
        .create_workspace("acme", "box", vec![repository("ghe:acme/server")])
        .await
        .unwrap();
    let created = laptop
        .create_codespace(NewCodespace {
            workspace: "acme".into(),
            preset: Some("dev".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let create = loop {
        match tokio::time::timeout(WAIT, calls.recv())
            .await
            .unwrap()
            .unwrap()
        {
            HostCall::CreateInstance(create) => break create,
            _ => continue,
        }
    };
    let agent_key = ClientIdentity::generate().unwrap();
    let endpoint = pair_oracle(&create.bootstrap.link, &agent_key, "agent", None)
        .await
        .unwrap();
    let (agent, _agent_events) = OracleClient::connect(&endpoint, &agent_key, "agent")
        .await
        .unwrap();
    let bundle = agent.bundle().await.unwrap();
    let ghe = &bundle.git.providers[0];
    assert_eq!(ghe.proxy_url.as_deref(), Some("http://10.0.0.1:7440/ghe"));
    assert!(ghe.gh.is_none());

    let direct = agent
        .git_credential(&format!("{web}/acme/server.git"))
        .await
        .unwrap();
    assert_eq!(direct.token, None);
    let proxied = agent
        .git_credential(&format!("{proxy}/ghe/acme/server.git"))
        .await
        .unwrap()
        .token
        .unwrap();
    assert_eq!(proxied.username, created.id);
    assert_ne!(proxied.password.expose(), "ghp_real");
    let secret = proxied.password.expose().to_owned();

    let request = |secret: &str, owner: &str| GitProxyRequest {
        codespace: created.id.clone(),
        secret: Secret::new(secret),
        provider: "ghe".into(),
        owner: owner.into(),
        repository: "server.git".into(),
    };
    let access = host
        .git_proxy_access(request(&secret, "acme"))
        .await
        .unwrap();
    assert_eq!(access.upstream, format!("{web}/acme/server.git"));
    assert_eq!(access.token.unwrap().password.expose(), "ghp_real");
    let other = host
        .git_proxy_access(request(&secret, "other"))
        .await
        .unwrap();
    assert_eq!(other.upstream, format!("{web}/other/server.git"));
    assert_eq!(other.token, None);

    Mock::given(method("GET"))
        .and(path("/repos/acme/server/pulls"))
        .and(header("authorization", "Bearer ghp_real"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&hub)
        .await;
    let api = |url: String, method: &str, path: &str| CodespaceApiRequest {
        url,
        method: method.into(),
        path: path.into(),
        query: vec![("head".into(), "acme:loom/x".into())],
        body: None,
    };
    let server = format!("{web}/acme/server.git");
    let response = agent
        .codespace_api_request(api(server.clone(), "GET", "/repos/acme/server/pulls"))
        .await
        .unwrap();
    assert_eq!(response["status"], 200);
    for (url, method, path, expected) in [
        (server.clone(), "GET", "/user", ErrorCode::Forbidden),
        (
            server.clone(),
            "GET",
            "/repos/acme/other/pulls",
            ErrorCode::Forbidden,
        ),
        (
            server.clone(),
            "PATCH",
            "/repos/acme/server/pulls/x",
            ErrorCode::Forbidden,
        ),
        (
            server.clone(),
            "POST",
            "/repos/acme/server/pulls/1",
            ErrorCode::Forbidden,
        ),
        (
            format!("{web}/other/server.git"),
            "GET",
            "/repos/other/server/pulls",
            ErrorCode::BadRequest,
        ),
    ] {
        assert_eq!(
            code(
                agent
                    .codespace_api_request(api(url, method, path))
                    .await
                    .unwrap_err()
            ),
            expected,
            "{method} {path}"
        );
    }
    hub.verify().await;
    for (client, request, expected) in [
        (&host, request("wrong", "acme"), ErrorCode::Forbidden),
        (&host, request(&secret, ".."), ErrorCode::BadRequest),
        (&agent, request(&secret, "acme"), ErrorCode::Forbidden),
    ] {
        assert_eq!(
            code(client.git_proxy_access(request).await.unwrap_err()),
            expected
        );
    }
    let (other_host, _other_calls) = connect_host(&oracle, "other").await;
    assert_eq!(
        code(
            other_host
                .git_proxy_access(request(&secret, "acme"))
                .await
                .unwrap_err()
        ),
        ErrorCode::Forbidden
    );
}

async fn host_call(calls: &mut OracleCalls) -> (u64, HostCall) {
    tokio::time::timeout(WAIT, calls.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn volumes_require_host_confirmation_and_stay_reserved_until_close() {
    let oracle = start_oracle().await;
    let (laptop, mut events) = oracle.connect(Role::Device, "laptop").await;
    laptop.subscribe().await.unwrap();
    let (host, mut calls) = connect_host(&oracle, "box").await;
    let (other, mut other_calls) = connect_host(&oracle, "other").await;
    let cache = VolumeInput {
        name: "cache".into(),
        kind: VolumeKind::Incus,
        size_mib: Some(128),
        path: None,
    };
    let directory = VolumeInput {
        name: "data".into(),
        kind: VolumeKind::Directory,
        size_mib: None,
        path: Some("/srv/data".into()),
    };
    laptop.create_volume(directory.clone()).await.unwrap();
    laptop.create_volume(cache.clone()).await.unwrap();
    assert_eq!(
        code(laptop.create_volume(cache).await.unwrap_err()),
        ErrorCode::Conflict
    );
    let repos = vec![repository("https://example.com/team/app.git")];
    let mounts = vec![
        WorkspaceVolume {
            volume: "cache".into(),
            path: "/workspaces/cache".into(),
            read_only: false,
        },
        WorkspaceVolume {
            volume: "data".into(),
            path: "/workspaces/data".into(),
            read_only: true,
        },
    ];
    let mut invalid = mounts.clone();
    invalid[0].path = "/workspaces/app/target".into();
    assert_eq!(
        code(
            laptop
                .create_workspace_with_volumes("acme", "box", repos.clone(), invalid)
                .await
                .unwrap_err()
        ),
        ErrorCode::BadRequest
    );
    let confirmed = json!({"volumes": true, "directories": [{"name": "/srv/data"}]});
    for response in [json!({}), json!({"volumes": true}), confirmed.clone()] {
        let expected = response == confirmed;
        let create =
            laptop.create_workspace_with_volumes("acme", "box", repos.clone(), mounts.clone());
        let answer = async {
            let (id, call) = host_call(&mut calls).await;
            assert!(
                matches!(call, HostCall::CheckWorkspace(check) if check.directories == ["/srv/data"])
            );
            host.reply(id, Ok(response)).await.unwrap();
        };
        let (result, ()) = tokio::join!(create, answer);
        if expected {
            assert_eq!(result.unwrap().volumes, mounts);
        } else {
            assert_eq!(code(result.unwrap_err()), ErrorCode::Unavailable);
            assert!(laptop.workspaces().await.unwrap().is_empty());
        }
    }
    let err = laptop.delete_volume("cache").await.unwrap_err();
    assert!(message(err).contains("workspaces acme"));
    for (name, client, calls) in [
        ("box", &host, &mut calls),
        ("other", &other, &mut other_calls),
    ] {
        if name == "other" {
            let update = laptop.update_workspace("acme", Some(name), None, None);
            let answer = async {
                let (id, call) = host_call(calls).await;
                assert!(matches!(call, HostCall::CheckWorkspace(_)));
                client.reply(id, Ok(confirmed.clone())).await.unwrap();
            };
            let (result, ()) = tokio::join!(update, answer);
            assert_eq!(result.unwrap().host, name);
        }
        let create = laptop.create_codespace_with_warnings(NewCodespace {
            workspace: "acme".into(),
            ..Default::default()
        });
        let answer = async {
            let (id, call) = host_call(calls).await;
            assert!(matches!(call, HostCall::CheckWorkspace(_)));
            client.reply(id, Ok(confirmed.clone())).await.unwrap();
            let (id, call) = host_call(calls).await;
            let HostCall::CreateInstance(request) = call else {
                panic!("unexpected {call:?}")
            };
            assert_eq!(request.volumes[0].size_mib, Some(128));
            assert_eq!(request.volumes[1].source.as_deref(), Some("/srv/data"));
            assert!(request.volumes[1].read_only);
            client.reply(id, Ok(json!({}))).await.unwrap();
        };
        let (result, ()) = tokio::join!(create, answer);
        let codespace = result.unwrap().codespace;
        if name == "other" {
            laptop
                .update_workspace("acme", None, None, Some(vec![]))
                .await
                .unwrap();
            for name in ["cache", "data"] {
                let err = laptop.delete_volume(name).await.unwrap_err();
                assert!(message(err).contains(&codespace.id));
            }
        }
        laptop.close_codespace(&codespace.id).await.unwrap();
        let (id, call) = host_call(calls).await;
        assert!(
            matches!(call, HostCall::DeleteInstance { codespace: closed, .. } if closed == codespace.id)
        );
        client.reply(id, Ok(json!({}))).await.unwrap();
        next_event(&mut events, |e| {
            matches!(e, Event::CodespaceChanged { codespace: c }
            if c.id == codespace.id && c.state == CodespaceState::Closed)
        })
        .await;
    }
    let volumes = laptop.volumes().await.unwrap();
    assert_eq!(volumes[0].hosts, vec!["box", "other"]);
    assert!(!volumes[0].deleting);
    assert!(volumes[1].hosts.is_empty());
    let sizes = laptop.host_volumes("box");
    let answer = async {
        let (id, call) = host_call(&mut calls).await;
        assert!(matches!(&call, HostCall::VolumeUsage { volumes } if volumes == &["cache"]));
        host.reply(
            id,
            Ok(json!({"volumes": [{"name": "cache", "used_bytes": 4096}]})),
        )
        .await
        .unwrap();
    };
    let (sizes, ()) = tokio::join!(sizes, answer);
    let sizes = sizes.unwrap();
    assert_eq!(sizes.len(), 1);
    assert_eq!(
        (sizes[0].name.as_str(), sizes[0].used_bytes),
        ("cache", Some(4096))
    );
    laptop.delete_volume("data").await.unwrap();

    let delete = laptop.delete_volume("cache");
    let answer = async {
        let (id, call) = host_call(&mut calls).await;
        assert!(matches!(call, HostCall::DeleteVolume { volume } if volume == "cache"));
        host.reply(id, Ok(json!({}))).await.unwrap();
        let (id, call) = host_call(&mut other_calls).await;
        assert!(matches!(call, HostCall::DeleteVolume { volume } if volume == "cache"));
        let err = laptop
            .update_workspace("acme", None, None, Some(vec![mounts[0].clone()]))
            .await
            .unwrap_err();
        assert!(message(err).contains("being deleted"));
        other
            .reply(
                id,
                Err(OracleError::new(
                    ErrorCode::Conflict,
                    "the volume is in use",
                )),
            )
            .await
            .unwrap();
    };
    let (result, ()) = tokio::join!(delete, answer);
    assert_eq!(code(result.unwrap_err()), ErrorCode::Conflict);
    let volumes = laptop.volumes().await.unwrap();
    assert_eq!(volumes[0].hosts, vec!["other"]);
    assert!(volumes[0].deleting);
    let retry = laptop.delete_volume("cache");
    let answer = async {
        let (id, call) = host_call(&mut other_calls).await;
        assert!(matches!(call, HostCall::DeleteVolume { volume } if volume == "cache"));
        other.reply(id, Ok(json!({}))).await.unwrap();
    };
    let (result, ()) = tokio::join!(retry, answer);
    result.unwrap();
    assert!(laptop.volumes().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_keep_their_checks_and_ssh_destination() {
    let oracle = start_oracle().await;
    let (laptop, mut events) = oracle.connect(Role::Device, "laptop").await;
    laptop.subscribe().await.unwrap();
    let (host, mut calls) = connect_host(&oracle, "box").await;
    let check = |name: &str, status| HostCheck {
        name: name.into(),
        status,
        detail: format!("{name} detail"),
    };
    host.host_report(HostReport {
        version: "test".into(),
        driver: "incus".into(),
        cpus: 4,
        memory_mib: 8192,
        checks: vec![check("incus", HostCheckStatus::Ok)],
        ..Default::default()
    })
    .await
    .unwrap();
    let hosts = laptop.hosts().await.unwrap();
    assert_eq!(hosts[0].checks, vec![check("incus", HostCheckStatus::Ok)]);
    assert!(hosts[0].checked_at.is_some());
    assert!(hosts[0].report.as_ref().unwrap().checks.is_empty());

    let run = laptop.check_host("box", true);
    let answer = async {
        let (id, call) = host_call(&mut calls).await;
        assert!(matches!(call, HostCall::Check { docker: true }));
        host.reply(
            id,
            Ok(json!({"checks": [
                {"name": "incus", "status": "ok", "detail": "incus detail"},
                {"name": "docker", "status": "failed", "detail": "docker detail"},
            ]})),
        )
        .await
        .unwrap();
    };
    let (checked, ()) = tokio::join!(run, answer);
    let checked = checked.unwrap();
    assert_eq!(checked.checks[1], check("docker", HostCheckStatus::Failed));
    next_event(
        &mut events,
        |e| matches!(e, Event::HostChanged { host } if host.checks.len() == 2),
    )
    .await;
    assert!(laptop.host_volumes("box").await.unwrap().is_empty());

    let set = laptop.set_host_ssh("box", Some("dev@box.example")).await;
    assert_eq!(set.unwrap().ssh.as_deref(), Some("dev@box.example"));
    for bad in ["-oProxyCommand=x", "dev@box example", "a'b"] {
        let err = laptop.set_host_ssh("box", Some(bad)).await.unwrap_err();
        assert_eq!(code(err), ErrorCode::BadRequest, "{bad}");
    }
    assert_eq!(
        laptop.hosts().await.unwrap()[0].ssh.as_deref(),
        Some("dev@box.example")
    );
    assert_eq!(laptop.set_host_ssh("box", None).await.unwrap().ssh, None);
    assert_eq!(
        code(laptop.set_host_ssh("nobody", None).await.unwrap_err()),
        ErrorCode::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn model_accounts_secrets_and_presets_reach_the_codespace() {
    let oracle = start_oracle().await;
    let (laptop, mut events) = oracle.connect(Role::Device, "laptop").await;
    laptop.subscribe().await.unwrap();
    let (_host, mut calls) = start_host(&oracle).await;
    let files = MockServer::start().await;

    let key = laptop
        .set_model_api_key("key", "openai", "sk-test")
        .await
        .unwrap();
    assert_eq!(key.kind, AccountKind::ApiKey);
    assert_eq!(key.provider, "openai");
    next_event(
        &mut events,
        |e| matches!(e, Event::ModelAccountChanged { account } if account.name == "key"),
    )
    .await;
    laptop
        .set_model_api_key("mine", "my-llm", "sk-mine")
        .await
        .unwrap();
    let err = laptop
        .set_model_api_key("key", "anthropic", "sk-other")
        .await
        .unwrap_err();
    assert!(message(err).contains("is for openai"));
    let err = laptop
        .start_model_login(
            "mine",
            "my-llm",
            ModelLoginMethod::Browser { redirect_uri: None },
        )
        .await
        .unwrap_err();
    assert_eq!(code(err), ErrorCode::BadRequest);
    assert_eq!(
        code(laptop.set_secret("npm", "a\nb").await.unwrap_err()),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(laptop.set_secret("Npm", "x").await.unwrap_err()),
        ErrorCode::BadRequest
    );
    laptop.set_secret("npm", "npm-1").await.unwrap();

    for (yaml, expected) in [
        (
            "llm:\n  accounts:\n    openai: nobody\n",
            "no such model account",
        ),
        (
            "llm:\n  accounts:\n    anthropic: key\n",
            "which is an account of openai",
        ),
        (
            "llm:\n  default_model: anthropic/claude\n",
            "needs an account",
        ),
        (
            "environment:\n  secrets:\n    NPM_TOKEN: missing\n",
            "no such secret",
        ),
        ("storage:\n  data_dir: /tmp\n", "storage"),
    ] {
        let err = laptop.set_preset("dev", yaml).await.unwrap_err();
        assert!(message(err).contains(expected), "{yaml}");
    }
    let bad_file = PresetFile {
        path: "AGENTS.md".into(),
        content: "x".into(),
    };
    assert_eq!(
        code(
            laptop
                .set_full_preset("dev", "", None, Some(vec![bad_file]))
                .await
                .unwrap_err()
        ),
        ErrorCode::BadRequest
    );

    let yaml = "llm:\n  default_model: openai/gpt-test\n  accounts:\n    openai: key\n    my-llm: mine\n  providers:\n    my-llm:\n      type: openai-compatible\n      base_url: http://127.0.0.1:1\nenvironment:\n  secrets:\n    NPM_TOKEN: npm\n    NODE_AUTH_TOKEN: npm\n";
    let skill = PresetFile {
        path: "skills/review/SKILL.md".into(),
        content: "---\nname: review\ndescription: Review.\n---\n".into(),
    };
    let preset = laptop
        .set_full_preset("dev", yaml, Some("RULES"), Some(vec![skill.clone()]))
        .await
        .unwrap();
    assert_eq!(preset.model_accounts["openai"], "key");
    assert_eq!(preset.secrets["NODE_AUTH_TOKEN"], "npm");
    assert_eq!(preset.files, vec!["skills/review/SKILL.md".to_owned()]);
    let detail = laptop.preset("dev").await.unwrap();
    assert_eq!(detail.preset.agents_md, "RULES");
    assert_eq!(detail.files, vec![skill.clone()]);
    let kept = laptop.set_preset("dev", yaml).await.unwrap();
    assert_eq!(kept.agents_md, "RULES");
    assert_eq!(kept.files.len(), 1);
    let accounts = laptop.model_accounts().await.unwrap();
    assert_eq!(accounts[0].presets, vec!["dev".to_owned()]);
    let secrets = laptop.secrets().await.unwrap();
    assert_eq!(secrets[0].presets, vec!["dev".to_owned()]);
    assert_eq!(
        code(laptop.delete_secret("npm").await.unwrap_err()),
        ErrorCode::Conflict
    );
    assert_eq!(
        code(laptop.delete_model_account("key").await.unwrap_err()),
        ErrorCode::Conflict
    );

    laptop
        .create_workspace(
            "acme",
            "box",
            vec![repository(&format!("{}/app.git", files.uri()))],
        )
        .await
        .unwrap();
    laptop.set_workspace_notes("acme", "NOTES").await.unwrap();
    let empty = FirstMessage {
        text: " ".into(),
        model: None,
    };
    let err = laptop
        .create_codespace(NewCodespace {
            workspace: "acme".into(),
            first_message: Some(empty),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(code(err), ErrorCode::BadRequest);
    let created = laptop
        .create_codespace(NewCodespace {
            workspace: "acme".into(),
            preset: Some("dev".into()),
            first_message: Some(FirstMessage {
                text: "Start".into(),
                model: Some("openai/gpt-test".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(created.first_message);
    let create = loop {
        match tokio::time::timeout(WAIT, calls.recv())
            .await
            .unwrap()
            .unwrap()
        {
            HostCall::CreateInstance(create) => break create,
            _ => continue,
        }
    };
    let agent_key = ClientIdentity::generate().unwrap();
    let endpoint = pair_oracle(&create.bootstrap.link, &agent_key, "agent", None)
        .await
        .unwrap();
    let (agent, mut agent_events) = OracleClient::connect(&endpoint, &agent_key, "agent")
        .await
        .unwrap();
    agent.subscribe().await.unwrap();

    let bundle = agent.bundle().await.unwrap();
    assert_eq!(bundle.notes.as_deref(), Some("NOTES"));
    assert_eq!(bundle.agent.name, "dev");
    assert_eq!(bundle.agent.agents_md, "RULES");
    assert_eq!(bundle.agent.files, vec![skill]);
    assert_eq!(bundle.agent.secrets["NPM_TOKEN"].expose(), "npm-1");
    assert_eq!(bundle.agent.secrets["NODE_AUTH_TOKEN"].expose(), "npm-1");
    assert_eq!(bundle.agent.accounts["my-llm"], "mine");

    let lent = agent.model_credentials().await.unwrap();
    assert!(lent.errors.is_empty(), "{:?}", lent.errors);
    assert_eq!(lent.sections["openai"]["api_key"], "sk-test");
    assert_eq!(lent.sections["my-llm"], json!({"api_key": "sk-mine"}));
    assert_eq!(lent.accounts["openai"], "key");
    assert_eq!(lent.valid_secs, None);
    assert_eq!(
        code(laptop.model_credentials().await.unwrap_err()),
        ErrorCode::Forbidden
    );

    let first = agent.take_first_message().await.unwrap().unwrap();
    assert_eq!(first.text, "Start");
    assert_eq!(first.model.as_deref(), Some("openai/gpt-test"));
    assert!(agent.take_first_message().await.unwrap().is_none());

    laptop.set_secret("npm", "npm-2").await.unwrap();
    let changed = next_event(&mut agent_events, |e| {
        matches!(e, Event::ConfigChanged { .. })
    })
    .await;
    assert_eq!(
        changed,
        Event::ConfigChanged {
            presets: vec!["dev".into()]
        }
    );
    let bundle = agent.bundle().await.unwrap();
    assert_eq!(bundle.agent.secrets["NPM_TOKEN"].expose(), "npm-2");

    agent.codespace_notes("CHANGED").await.unwrap();
    assert_eq!(
        laptop.workspace_notes("acme").await.unwrap().text,
        "CHANGED"
    );
    let summary = |id: &str, state: &str| SessionSummary {
        id: id.into(),
        title: Some(format!("Session {id}")),
        state: state.into(),
        archived: false,
        model: None,
        created_at: 1,
        updated_at: 2,
    };
    agent
        .session_summaries(
            vec![summary("a", "thinking"), summary("b", "idle")],
            Vec::new(),
            true,
        )
        .await
        .unwrap();
    next_event(
        &mut events,
        |e| matches!(e, Event::SessionChanged { session } if session.session.id == "b"),
    )
    .await;
    agent
        .session_summaries(vec![summary("a", "completed")], Vec::new(), true)
        .await
        .unwrap();
    next_event(
        &mut events,
        |e| matches!(e, Event::SessionRemoved { id, .. } if id == "b"),
    )
    .await;
    let sessions = laptop.sessions(Some(&created.id)).await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.state, "completed");
    assert_eq!(sessions[0].workspace, "acme");
}
