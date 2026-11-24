use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, LINK};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const GITHUB_WEB_URL: &str = "https://github.com";
pub const GITHUB_API_URL: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const TIMEOUT: Duration = Duration::from_secs(30);
const REACH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 8 << 20;
const EXPIRATION_HEADER: &str = "github-authentication-token-expiration";
const SCOPES_HEADER: &str = "x-oauth-scopes";

pub fn default_api_url(web_url: &str) -> String {
    let web = web_url.trim_end_matches('/');
    if web.eq_ignore_ascii_case(GITHUB_WEB_URL) {
        GITHUB_API_URL.to_owned()
    } else {
        format!("{web}/api/v3")
    }
}

pub fn repository_path(path: &str) -> Option<(String, String)> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(name)).then(|| (owner.to_owned(), name.to_owned()))
}

pub fn repository_of(web_url: &str, url: &str) -> Option<(String, String)> {
    let head = url.get(..web_url.len())?;
    if !head.eq_ignore_ascii_case(web_url) {
        return None;
    }
    let rest = &url[web_url.len()..];
    rest.starts_with('/')
        .then(|| repository_path(rest))
        .flatten()
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn parse_expiration(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let date: Vec<i64> = parts
        .next()?
        .split('-')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    let time: Vec<i64> = parts
        .next()?
        .split(':')
        .map(|n| n.parse().ok())
        .collect::<Option<_>>()?;
    let offset = match parts.next().unwrap_or("UTC") {
        "UTC" | "GMT" | "Z" => 0,
        zone => {
            let (sign, digits) = match zone.split_at_checked(1)? {
                ("+", rest) => (1, rest),
                ("-", rest) => (-1, rest),
                _ => return None,
            };
            if digits.len() != 4 {
                return None;
            }
            let hours: i64 = digits[..2].parse().ok()?;
            let minutes: i64 = digits[2..].parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let [year, month, day]: [i64; 3] = date.try_into().ok()?;
    let [hour, minute, second]: [i64; 3] = time.try_into().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
    Patch,
}

impl Method {
    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRequest {
    pub method: Method,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

impl ApiRequest {
    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            path: path.into(),
            query: Vec::new(),
            body: None,
        }
    }

    pub fn query(mut self, name: &str, value: impl Into<String>) -> Self {
        self.query.push((name.to_owned(), value.into()));
        self
    }

    pub fn with_body(mut self, method: Method, body: Value) -> Self {
        self.method = method;
        self.body = Some(body);
        self
    }
}

pub fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 1000
        && !path.contains("..")
        && !path.contains("//")
        && path
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '?' | '#' | '\\' | '%'))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiResponse {
    pub status: u16,
    #[serde(default)]
    pub body: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_expiration: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    #[serde(default)]
    pub has_next: bool,
}

impl ApiResponse {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn error(&self) -> GitHubError {
        let mut message = self
            .body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the request failed")
            .to_owned();
        if let Some(errors) = self.body.get("errors").and_then(Value::as_array) {
            let details: Vec<&str> = errors
                .iter()
                .filter_map(|e| e.get("message").and_then(Value::as_str))
                .collect();
            if !details.is_empty() {
                message = format!("{message}: {}", details.join("; "));
            }
        }
        GitHubError::Status {
            status: self.status,
            message,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitHubError {
    #[error("cannot reach {url}: {message}")]
    Unreachable { url: String, message: String },
    #[error("{message} (HTTP {status})")]
    Status { status: u16, message: String },
    #[error("unexpected answer from the API: {0}")]
    Decode(String),
    #[error("{0}")]
    Route(String),
}

impl GitHubError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

type Result<T> = std::result::Result<T, GitHubError>;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, request: &ApiRequest) -> Result<ApiResponse>;
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("loom/", env!("CARGO_PKG_VERSION")))
            .timeout(TIMEOUT)
            .build()
            .map_err(|err| GitHubError::Route(format!("cannot make an HTTP client: {err}")))?;
        Ok(Self { client })
    }

    pub async fn send(
        &self,
        api_url: &str,
        token: Option<&str>,
        request: &ApiRequest,
    ) -> Result<ApiResponse> {
        if !valid_path(&request.path) {
            return Err(GitHubError::Route(format!(
                "{:?} is not a path of the API",
                request.path
            )));
        }
        let text = format!("{}{}", api_url.trim_end_matches('/'), request.path);
        let mut url = reqwest::Url::parse(&text)
            .map_err(|err| GitHubError::Route(format!("{text:?} is not a URL: {err}")))?;
        if !request.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in &request.query {
                pairs.append_pair(name, value);
            }
        }
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
        };
        let mut builder = self
            .client
            .request(method, url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION);
        if let Some(token) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(body) = &request.body {
            let bytes =
                serde_json::to_vec(body).map_err(|err| GitHubError::Decode(err.to_string()))?;
            builder = builder.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let unreachable = |err: reqwest::Error| GitHubError::Unreachable {
            url: api_url.to_owned(),
            message: err.to_string(),
        };
        let response = builder.send().await.map_err(unreachable)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = response.bytes().await.map_err(unreachable)?;
        if bytes.len() > MAX_BODY {
            return Err(GitHubError::Decode(format!(
                "the answer has more than {MAX_BODY} bytes"
            )));
        }
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(
                |_| json!({"message": String::from_utf8_lossy(&bytes[..bytes.len().min(500)])}),
            )
        };
        Ok(ApiResponse {
            status,
            body,
            token_expiration: header(&headers, EXPIRATION_HEADER)
                .as_deref()
                .and_then(parse_expiration),
            scopes: header(&headers, SCOPES_HEADER),
            has_next: header(&headers, LINK.as_str())
                .is_some_and(|link| link.contains("rel=\"next\"")),
        })
    }

    pub async fn reach(&self, api_url: &str) -> std::result::Result<(), String> {
        let url = reqwest::Url::parse(api_url).map_err(|err| err.to_string())?;
        self.client
            .get(url)
            .header(ACCEPT, "application/vnd.github+json")
            .timeout(REACH_TIMEOUT)
            .send()
            .await
            .map(|_| ())
            .map_err(|err| {
                let mut message = err.to_string();
                let mut source = std::error::Error::source(&err);
                while let Some(inner) = source {
                    message = format!("{message}: {inner}");
                    source = inner.source();
                }
                message
            })
    }
}

pub struct Direct {
    http: Http,
    api_url: String,
    token: Option<String>,
}

impl Direct {
    pub fn new(http: Http, api_url: &str, token: Option<String>) -> Self {
        Self {
            http,
            api_url: api_url.to_owned(),
            token,
        }
    }
}

#[async_trait]
impl Transport for Direct {
    async fn send(&self, request: &ApiRequest) -> Result<ApiResponse> {
        self.http
            .send(&self.api_url, self.token.as_deref(), request)
            .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenOwner {
    pub login: String,
    pub id: u64,
    pub name: Option<String>,
    pub expires_at: Option<u64>,
    pub scopes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepository {
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub private: bool,
    pub default_branch: Option<String>,
    pub web_url: String,
    pub clone_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPullRequest {
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn remote_repository(value: &Value) -> Result<RemoteRepository> {
    let missing = |key: &str| GitHubError::Decode(format!("a repository has no {key}"));
    let owner = value
        .get("owner")
        .and_then(|owner| text(owner, "login"))
        .ok_or_else(|| missing("owner"))?;
    let name = text(value, "name").ok_or_else(|| missing("name"))?;
    Ok(RemoteRepository {
        web_url: text(value, "html_url").ok_or_else(|| missing("html_url"))?,
        clone_url: text(value, "clone_url").ok_or_else(|| missing("clone_url"))?,
        description: text(value, "description"),
        private: value
            .get("private")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        default_branch: text(value, "default_branch"),
        owner,
        name,
    })
}

fn pull_request(value: &Value) -> Result<PullRequest> {
    Ok(PullRequest {
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| GitHubError::Decode("a pull request has no number".into()))?,
        url: text(value, "html_url")
            .ok_or_else(|| GitHubError::Decode("a pull request has no html_url".into()))?,
        body: text(value, "body").unwrap_or_default(),
    })
}

fn segment(part: &str) -> Result<&str> {
    let valid = !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
        Ok(part)
    } else {
        Err(GitHubError::Route(format!(
            "{part:?} is not a name of an owner or a repository"
        )))
    }
}

#[derive(Clone)]
pub struct GitHub {
    transport: Arc<dyn Transport>,
}

impl GitHub {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }

    pub fn direct(http: Http, api_url: &str, token: Option<String>) -> Self {
        Self::new(Arc::new(Direct::new(http, api_url, token)))
    }

    pub async fn send(&self, request: &ApiRequest) -> Result<ApiResponse> {
        let response = self.transport.send(request).await?;
        if response.is_success() {
            Ok(response)
        } else {
            Err(response.error())
        }
    }

    pub async fn user(&self) -> Result<TokenOwner> {
        let response = self.send(&ApiRequest::get("/user")).await?;
        let body = &response.body;
        Ok(TokenOwner {
            login: text(body, "login")
                .ok_or_else(|| GitHubError::Decode("the user has no login".into()))?,
            id: body.get("id").and_then(Value::as_u64).unwrap_or_default(),
            name: text(body, "name").filter(|name| !name.is_empty()),
            expires_at: response.token_expiration,
            scopes: response.scopes,
        })
    }

    async fn repositories(
        &self,
        request: ApiRequest,
        items: impl Fn(&Value) -> Option<&Vec<Value>>,
    ) -> Result<Page<RemoteRepository>> {
        let response = self.send(&request).await?;
        let list = items(&response.body)
            .ok_or_else(|| GitHubError::Decode("the answer has no list of repositories".into()))?;
        Ok(Page {
            items: list
                .iter()
                .map(remote_repository)
                .collect::<Result<Vec<_>>>()?,
            more: response.has_next,
        })
    }

    pub async fn search_repositories(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Page<RemoteRepository>> {
        let request = ApiRequest::get("/search/repositories")
            .query("q", query)
            .query("per_page", limit.clamp(1, 100).to_string());
        self.repositories(request, |body| body.get("items").and_then(Value::as_array))
            .await
    }

    pub async fn user_repositories(&self, limit: u32) -> Result<Page<RemoteRepository>> {
        let request = ApiRequest::get("/user/repos")
            .query("sort", "updated")
            .query("per_page", limit.clamp(1, 100).to_string());
        self.repositories(request, Value::as_array).await
    }

    pub async fn repository(&self, owner: &str, name: &str) -> Result<RemoteRepository> {
        let path = format!("/repos/{}/{}", segment(owner)?, segment(name)?);
        let response = self.send(&ApiRequest::get(path)).await?;
        remote_repository(&response.body)
    }

    pub async fn branches(&self, owner: &str, name: &str, limit: u32) -> Result<Page<String>> {
        let path = format!("/repos/{}/{}/branches", segment(owner)?, segment(name)?);
        let request = ApiRequest::get(path).query("per_page", limit.clamp(1, 100).to_string());
        let response = self.send(&request).await?;
        let list = response
            .body
            .as_array()
            .ok_or_else(|| GitHubError::Decode("the answer has no list of branches".into()))?;
        Ok(Page {
            items: list.iter().filter_map(|b| text(b, "name")).collect(),
            more: response.has_next,
        })
    }

    pub async fn find_pull_request(
        &self,
        owner: &str,
        name: &str,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let path = format!("/repos/{}/{}/pulls", segment(owner)?, segment(name)?);
        let request = ApiRequest::get(path)
            .query("head", head)
            .query("state", "open");
        let response = self.send(&request).await?;
        let list = response
            .body
            .as_array()
            .ok_or_else(|| GitHubError::Decode("the answer has no list of pull requests".into()))?;
        list.first().map(pull_request).transpose()
    }

    pub async fn create_pull_request(
        &self,
        owner: &str,
        name: &str,
        new: &NewPullRequest,
    ) -> Result<PullRequest> {
        let path = format!("/repos/{}/{}/pulls", segment(owner)?, segment(name)?);
        let body = json!({
            "title": new.title,
            "body": new.body,
            "head": new.head,
            "base": new.base,
            "draft": new.draft,
        });
        let response = self
            .send(&ApiRequest::get(path).with_body(Method::Post, body))
            .await?;
        pull_request(&response.body)
    }

    pub async fn update_pull_request(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        body: &str,
    ) -> Result<PullRequest> {
        let path = format!(
            "/repos/{}/{}/pulls/{number}",
            segment(owner)?,
            segment(name)?
        );
        let response = self
            .send(&ApiRequest::get(path).with_body(Method::Patch, json!({"body": body})))
            .await?;
        pull_request(&response.body)
    }
}
