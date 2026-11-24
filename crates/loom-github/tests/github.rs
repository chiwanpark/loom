use loom_github::{
    ApiRequest, GitHub, GitHubError, Http, Method, NewPullRequest, default_api_url,
    parse_expiration, repository_path, valid_path,
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
