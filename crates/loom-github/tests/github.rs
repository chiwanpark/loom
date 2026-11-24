use loom_github::{
    ApiRequest, GitHub, GitHubError, Http, Method, NewPullRequest, app_jwt, default_api_url,
    parse_expiration, parse_timestamp, repository_path, valid_path,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn repo(owner: &str, name: &str, private: bool) -> serde_json::Value {
    json!({
        "name": name,
        "owner": {"login": owner},
        "private": private,
        "description": "a repository",
        "default_branch": "main",
        "html_url": format!("https://github.com/{owner}/{name}"),
        "clone_url": format!("https://github.com/{owner}/{name}.git"),
    })
}

#[test]
fn the_api_url_follows_the_web_url() {
    assert_eq!(
        default_api_url("https://github.com/"),
        "https://api.github.com"
    );
    assert_eq!(
        default_api_url("https://ghe.example.com"),
        "https://ghe.example.com/api/v3"
    );
}

#[test]
fn repository_paths_have_an_owner_and_a_name() {
    assert_eq!(
        repository_path("/acme/server.git"),
        Some(("acme".into(), "server".into()))
    );
    assert_eq!(
        repository_path("acme/web.app"),
        Some(("acme".into(), "web.app".into()))
    );
    for bad in ["acme", "acme/", "/x/..", "a/b/c", "a b/c"] {
        assert_eq!(repository_path(bad), None, "{bad}");
    }
}

#[test]
fn expiration_dates_become_unix_seconds() {
    assert_eq!(parse_expiration("1970-01-01 00:00:00 UTC"), Some(0));
    assert_eq!(
        parse_expiration("2000-03-01 00:00:00 UTC"),
        Some(951_868_800)
    );
    assert_eq!(
        parse_expiration("2000-03-01 00:00:00 -0700"),
        Some(951_868_800 + 7 * 3600)
    );
    assert_eq!(
        parse_expiration("2000-03-01 09:30:00 +0930"),
        Some(951_868_800)
    );
    for bad in [
        "",
        "2000-03-01",
        "2000-13-01 00:00:00 UTC",
        "x y z",
        "2000-03-01 00:00:00 PST",
    ] {
        assert_eq!(parse_expiration(bad), None, "{bad}");
    }
}

#[test]
fn paths_stay_in_the_api() {
    assert!(valid_path("/repos/acme/server/pulls"));
    for bad in [
        "repos",
        "/a/../b",
        "//evil.example.com/x",
        "/a?b=1",
        "/a b",
        "/a%2e%2e/b",
    ] {
        assert!(!valid_path(bad), "{bad}");
    }
}

#[tokio::test]
async fn the_user_of_a_token_has_a_login_and_an_expiry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer secret"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "github-authentication-token-expiration",
                    "2030-01-01 00:00:00 UTC",
                )
                .set_body_json(json!({"login": "octocat", "id": 7, "name": "The Octocat"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        )
        .mount(&server)
        .await;
    let http = Http::new().unwrap();
    let github = GitHub::direct(http.clone(), &server.uri(), Some("secret".into()));
    let owner = github.user().await.unwrap();
    assert_eq!(owner.login, "octocat");
    assert_eq!(owner.id, 7);
    assert_eq!(owner.name.as_deref(), Some("The Octocat"));
    assert_eq!(
        owner.expires_at,
        parse_expiration("2030-01-01 00:00:00 UTC")
    );

    let wrong = GitHub::direct(http, &server.uri(), Some("wrong".into()));
    let err = wrong.user().await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert_eq!(err.to_string(), "Bad credentials (HTTP 401)");
}

#[tokio::test]
async fn repositories_and_branches_come_from_the_api() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/repositories"))
        .and(query_param("q", "server org:acme"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", "<https://x/search?page=2>; rel=\"next\"")
                .set_body_json(json!({"total_count": 2, "items": [repo("acme", "server", true)]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([repo("me", "tool", false)])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/server/branches"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([{"name": "main"}, {"name": "dev"}])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo("acme", "server", true)))
        .mount(&server)
        .await;
    let github = GitHub::direct(Http::new().unwrap(), &server.uri(), None);
    let found = github
        .search_repositories("server org:acme", 20)
        .await
        .unwrap();
    assert!(found.more);
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].owner, "acme");
    assert!(found.items[0].private);
    assert_eq!(
        found.items[0].clone_url,
        "https://github.com/acme/server.git"
    );
    let mine = github.user_repositories(50).await.unwrap();
    assert!(!mine.more);
    assert_eq!(mine.items[0].name, "tool");
    let branches = github.branches("acme", "server", 100).await.unwrap();
    assert_eq!(branches.items, vec!["main".to_owned(), "dev".to_owned()]);
    let repository = github.repository("acme", "server").await.unwrap();
    assert_eq!(repository.default_branch.as_deref(), Some("main"));
    assert!(matches!(
        github.branches("acme", "..", 1).await,
        Err(GitHubError::Route(_))
    ));
}

#[tokio::test]
async fn pull_requests_are_found_created_and_changed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/server/pulls"))
        .and(query_param("head", "acme:loom/c1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/server/pulls"))
        .and(body_json(json!({
            "title": "Fix", "body": "Text", "head": "loom/c1", "base": "main", "draft": true
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": 12, "html_url": "https://github.com/acme/server/pull/12", "body": "Text"
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/server/pulls/12"))
        .and(body_json(json!({"body": "Text\n\nMore"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 12, "html_url": "https://github.com/acme/server/pull/12", "body": "Text\n\nMore"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/other/pulls"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{"message": "No commits between main and loom/c1"}]
        })))
        .mount(&server)
        .await;
    let github = GitHub::direct(Http::new().unwrap(), &server.uri(), Some("t".into()));
    assert_eq!(
        github
            .find_pull_request("acme", "server", "acme:loom/c1")
            .await
            .unwrap(),
        None
    );
    let new = NewPullRequest {
        title: "Fix".into(),
        body: "Text".into(),
        head: "loom/c1".into(),
        base: "main".into(),
        draft: true,
    };
    let created = github
        .create_pull_request("acme", "server", &new)
        .await
        .unwrap();
    assert_eq!(created.number, 12);
    let changed = github
        .update_pull_request("acme", "server", 12, "Text\n\nMore")
        .await
        .unwrap();
    assert_eq!(changed.body, "Text\n\nMore");
    let err = github
        .create_pull_request("acme", "other", &new)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Validation Failed: No commits between main and loom/c1 (HTTP 422)"
    );
}

#[tokio::test]
async fn raw_requests_keep_the_status_and_the_reach_test_needs_any_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/meta"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not json"))
        .mount(&server)
        .await;
    let http = Http::new().unwrap();
    let response = http
        .send(&server.uri(), None, &ApiRequest::get("/meta"))
        .await
        .unwrap();
    assert_eq!(response.status, 404);
    assert_eq!(response.body["message"], "not json");
    assert!(http.reach(&server.uri()).await.is_ok());
    assert!(http.reach("http://127.0.0.1:1").await.is_err());
    assert!(matches!(
        http.send(&server.uri(), None, &ApiRequest::get("//x"))
            .await,
        Err(GitHubError::Route(_))
    ));
    assert_eq!(Method::parse("patch"), Some(Method::Patch));
    assert_eq!(Method::parse("DELETE"), None);
}

pub fn rsa_key() -> String {
    rcgen::KeyPair::generate_rsa_for(&rcgen::PKCS_RSA_SHA256, rcgen::RsaKeySize::_2048)
        .unwrap()
        .serialize_pem()
}

fn claims(jwt: &str) -> serde_json::Value {
    use base64::Engine;
    let part = jwt.split('.').nth(1).unwrap();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn an_app_makes_tokens_for_its_installations() {
    let key = rsa_key();
    let jwt = app_jwt(42, &key, 1_000_000).unwrap();
    let claims = claims(&jwt);
    assert_eq!(claims["iss"], "42");
    assert_eq!(claims["iat"], 1_000_000 - 60);
    assert_eq!(claims["exp"], 1_000_000 + 540);
    assert!(matches!(
        app_jwt(42, "not a key", 0),
        Err(GitHubError::Route(_))
    ));

    let server = MockServer::start().await;
    let bearer = format!("Bearer {jwt}");
    Mock::given(method("GET"))
        .and(path("/app"))
        .and(header("authorization", bearer.as_str()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 42, "slug": "loom-bot", "name": "Loom"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/orgs/someone/installation"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/users/someone/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 7})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/orgs/acme/installation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 8})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/8/access_tokens"))
        .and(body_json(json!({"repositories": ["server"]})))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"token": "ghs_one", "expires_at": "2000-03-01T01:00:00Z"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/app/installations/8/access_tokens"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"token": "ghs_all"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(header("authorization", "Bearer ghs_all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"total_count": 1, "repositories": [repo("acme", "server", true)]}),
        ))
        .mount(&server)
        .await;

    let http = Http::new().unwrap();
    let app = GitHub::direct(http.clone(), &server.uri(), Some(jwt));
    assert_eq!(app.app().await.unwrap().slug, "loom-bot");
    assert_eq!(app.installation("someone").await.unwrap(), 7);
    assert_eq!(app.installation("acme").await.unwrap(), 8);
    let one = app
        .installation_token(8, &["server".to_owned()])
        .await
        .unwrap();
    assert_eq!(one.token, "ghs_one");
    assert_eq!(one.expires_at, Some(951_868_800 + 3600));
    assert!(
        app.installation_token(8, &["a/b".to_owned()])
            .await
            .is_err()
    );
    let all = app.installation_token(8, &[]).await.unwrap();
    assert_eq!(all.expires_at, None);
    let installation = GitHub::direct(http, &server.uri(), Some(all.token));
    let page = installation.installation_repositories(100).await.unwrap();
    assert_eq!(page.items[0].name, "server");
    assert_eq!(
        parse_timestamp("2000-03-01T00:00:00.123Z"),
        Some(951_868_800)
    );
    assert_eq!(parse_timestamp("2000-03-01"), None);
}
