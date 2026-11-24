use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, BodyStream, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use loom_client::OracleClient;
use loom_client::oracle_proto::{ErrorCode, GitProxyAccess, GitProxyRequest, OracleError, Secret};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::driver::Driver;

type Body = BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

const ACCESS_TTL: Duration = Duration::from_secs(300);
const ADDRESS_TTL: Duration = Duration::from_secs(60);
const EXPIRY_MARGIN: u64 = 60;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const BIND_RETRY: Duration = Duration::from_secs(10);
const REALM: &str = "Basic realm=\"Loom Git proxy\"";
const REQUEST_HEADERS: &[HeaderName] = &[
    header::ACCEPT,
    header::ACCEPT_ENCODING,
    header::CONTENT_ENCODING,
    header::CONTENT_LENGTH,
    header::CONTENT_TYPE,
    header::USER_AGENT,
];
const RESPONSE_HEADERS: &[HeaderName] = &[
    header::CACHE_CONTROL,
    header::CONTENT_ENCODING,
    header::CONTENT_LENGTH,
    header::CONTENT_TYPE,
    header::EXPIRES,
    header::PRAGMA,
];
const GIT_PROTOCOL: &str = "git-protocol";

#[async_trait]
pub trait ProxyBackend: Send + Sync {
    async fn access(&self, request: GitProxyRequest) -> Result<GitProxyAccess, OracleError>;

    async fn addresses(&self, codespace: &str) -> anyhow::Result<Vec<IpAddr>>;
}

pub struct HostBackend {
    pub driver: Arc<dyn Driver>,
    pub oracle: String,
    pub client: watch::Receiver<Option<OracleClient>>,
}

#[async_trait]
impl ProxyBackend for HostBackend {
    async fn access(&self, request: GitProxyRequest) -> Result<GitProxyAccess, OracleError> {
        let client = self.client.borrow().clone().ok_or_else(|| {
            OracleError::new(
                ErrorCode::Unavailable,
                "the host is not connected to the oracle",
            )
        })?;
        client
            .git_proxy_access(request)
            .await
            .map_err(|err| match err {
                loom_client::ClientError::Oracle(err) => err,
                other => OracleError::new(ErrorCode::Unavailable, other.to_string()),
            })
    }

    async fn addresses(&self, codespace: &str) -> anyhow::Result<Vec<IpAddr>> {
        self.driver.addresses(&self.oracle, codespace).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPath {
    pub provider: String,
    pub owner: String,
    pub repository: String,
    pub service: String,
}

const SERVICES: &[&str] = &["info/refs", "git-upload-pack", "git-receive-pack"];

pub fn parse_path(method: &Method, path: &str, query: Option<&str>) -> Option<GitPath> {
    let path = path.strip_prefix('/')?;
    let (provider, rest) = path.split_once('/')?;
    let (owner, rest) = rest.split_once('/')?;
    let (repository, service) = rest.split_once('/')?;
    if !SERVICES.contains(&service) {
        return None;
    }
    let expected = if service == "info/refs" {
        let services = ["service=git-upload-pack", "service=git-receive-pack"];
        if !query.is_some_and(|query| services.contains(&query)) {
            return None;
        }
        Method::GET
    } else {
        Method::POST
    };
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    (method == expected && valid(provider) && valid(owner) && valid(repository)).then(|| GitPath {
        provider: provider.to_owned(),
        owner: owner.to_owned(),
        repository: repository.to_owned(),
        service: service.to_owned(),
    })
}

pub fn basic_credentials(value: Option<&HeaderValue>) -> Option<(String, String)> {
    let value = value?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, password) = text.split_once(':')?;
    Some((user.to_owned(), password.to_owned()))
}

type AccessKey = (String, String, String, String, String);

pub struct GitProxy {
    backend: Arc<dyn ProxyBackend>,
    http: reqwest::Client,
    access: Mutex<HashMap<AccessKey, (Instant, GitProxyAccess)>>,
    addresses: Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>,
}

fn text(status: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(format!("{message}\n")))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

impl GitProxy {
    pub fn new(backend: Arc<dyn ProxyBackend>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("loom-host/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()?;
        Ok(Self {
            backend,
            http,
            access: Mutex::new(HashMap::new()),
            addresses: Mutex::new(HashMap::new()),
        })
    }

    async fn known_address(&self, codespace: &str, peer: IpAddr) -> bool {
        let cached = self
            .addresses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(codespace)
            .filter(|(at, _)| at.elapsed() < ADDRESS_TTL)
            .map(|(_, addresses)| addresses.contains(&peer));
        if cached == Some(true) {
            return true;
        }
        match self.backend.addresses(codespace).await {
            Ok(addresses) => {
                let found = addresses.contains(&peer);
                self.addresses
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(codespace.to_owned(), (Instant::now(), addresses));
                found
            }
            Err(err) => {
                tracing::warn!(%codespace, error = %format!("{err:#}"), "cannot read the addresses of a codespace");
                false
            }
        }
    }

    async fn access(
        &self,
        codespace: &str,
        secret: &str,
        path: &GitPath,
    ) -> Result<GitProxyAccess, OracleError> {
        let key = (
            codespace.to_owned(),
            secret.to_owned(),
            path.provider.clone(),
            path.owner.to_lowercase(),
            path.repository.to_lowercase(),
        );
        if let Some((until, access)) = self
            .access
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            && *until > Instant::now()
        {
            return Ok(access.clone());
        }
        let access = self
            .backend
            .access(GitProxyRequest {
                codespace: codespace.to_owned(),
                secret: Secret::new(secret),
                provider: path.provider.clone(),
                owner: path.owner.clone(),
                repository: path.repository.clone(),
            })
            .await?;
        let mut ttl = ACCESS_TTL;
        if let Some(expires_at) = access.token.as_ref().and_then(|t| t.expires_at) {
            let left = expires_at.saturating_sub(now_unix() + EXPIRY_MARGIN);
            ttl = ttl.min(Duration::from_secs(left));
        }
        let mut cache = self.access.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        cache.retain(|_, (until, _)| *until > now);
        if !ttl.is_zero() {
            cache.insert(key, (now + ttl, access.clone()));
        }
        Ok(access)
    }

    pub async fn handle(&self, peer: IpAddr, request: Request<Incoming>) -> Response<Body> {
        let peer = peer.to_canonical();
        let Some(path) = parse_path(
            request.method(),
            request.uri().path(),
            request.uri().query(),
        ) else {
            return text(
                StatusCode::NOT_FOUND,
                "the Git proxy of Loom serves only Git over HTTP",
            );
        };
        let Some((codespace, secret)) =
            basic_credentials(request.headers().get(header::AUTHORIZATION))
        else {
            let mut response = text(StatusCode::UNAUTHORIZED, "the Git proxy needs credentials");
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(REALM));
            return response;
        };
        if !self.known_address(&codespace, peer).await {
            tracing::warn!(%codespace, %peer, "refused a Git request from an address that is not of the codespace");
            return text(
                StatusCode::FORBIDDEN,
                "the request does not come from the codespace",
            );
        }
        let access = match self.access(&codespace, &secret, &path).await {
            Ok(access) => access,
            Err(err) => {
                tracing::info!(%codespace, error = %err.message, "the oracle refused a Git request");
                let status = match err.code {
                    ErrorCode::Forbidden => StatusCode::FORBIDDEN,
                    ErrorCode::NotFound => StatusCode::NOT_FOUND,
                    ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
                    _ => StatusCode::BAD_GATEWAY,
                };
                return text(status, &err.message);
            }
        };
        match self.forward(request, &path, &access).await {
            Ok(response) => {
                tracing::debug!(%codespace, upstream = %access.upstream, service = %path.service, status = %response.status(), "forwarded a Git request");
                response
            }
            Err(err) => {
                tracing::warn!(%codespace, upstream = %access.upstream, error = %err, "cannot forward a Git request");
                text(
                    StatusCode::BAD_GATEWAY,
                    &format!("cannot reach the Git provider: {err}"),
                )
            }
        }
    }

    async fn forward(
        &self,
        request: Request<Incoming>,
        path: &GitPath,
        access: &GitProxyAccess,
    ) -> Result<Response<Body>, reqwest::Error> {
        let mut url = format!("{}/{}", access.upstream, path.service);
        if let Some(query) = request.uri().query() {
            url.push('?');
            url.push_str(query);
        }
        let (parts, body) = request.into_parts();
        let mut builder = self.http.request(parts.method.clone(), url);
        for name in REQUEST_HEADERS
            .iter()
            .chain(std::iter::once(&HeaderName::from_static(GIT_PROTOCOL)))
        {
            for value in parts.headers.get_all(name) {
                builder = builder.header(name, value);
            }
        }
        if let Some(token) = &access.token {
            builder = builder.basic_auth(&token.username, Some(token.password.expose()));
        }
        if parts.method == Method::POST {
            let stream = BodyStream::new(body)
                .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) });
            builder = builder.body(reqwest::Body::wrap_stream(stream));
        }
        let upstream = builder.send().await?;
        let mut response = Response::builder().status(upstream.status().as_u16());
        for name in RESPONSE_HEADERS {
            for value in upstream.headers().get_all(name) {
                response = response.header(name, value);
            }
        }
        let stream = upstream
            .bytes_stream()
            .map_ok(Frame::data)
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>);
        Ok(response
            .body(BodyExt::boxed(StreamBody::new(stream)))
            .unwrap_or_else(|_| text(StatusCode::BAD_GATEWAY, "a malformed answer")))
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener, shutdown: CancellationToken) {
        loop {
            let (stream, peer) = tokio::select! {
                _ = shutdown.cancelled() => return,
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(err) => {
                        tracing::warn!(error = %err, "cannot accept a connection to the Git proxy");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                },
            };
            let proxy = self.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let proxy = proxy.clone();
                    async move { Ok::<_, Infallible>(proxy.handle(peer.ip(), request).await) }
                });
                let connection = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service);
                tokio::select! {
                    _ = shutdown.cancelled() => {}
                    result = connection => {
                        if let Err(err) = result {
                            tracing::debug!(error = %err, "a connection to the Git proxy failed");
                        }
                    }
                }
            });
        }
    }
}

pub async fn run(proxy: Arc<GitProxy>, listen: SocketAddr, shutdown: CancellationToken) {
    loop {
        match TcpListener::bind(listen).await {
            Ok(listener) => {
                tracing::info!(%listen, "the Git proxy is listening");
                proxy.serve(listener, shutdown).await;
                return;
            }
            Err(err) => {
                tracing::warn!(%listen, error = %err, "cannot listen for the Git proxy; trying again");
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(BIND_RETRY) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_git_paths_go_through() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(
            parse_path(
                &get,
                "/github/acme/server.git/info/refs",
                Some("service=git-upload-pack")
            ),
            Some(GitPath {
                provider: "github".into(),
                owner: "acme".into(),
                repository: "server".into(),
                service: "info/refs".into(),
            })
        );
        assert!(parse_path(&post, "/github/acme/server/git-receive-pack", None).is_some());
        for (method, path, query) in [
            (&get, "/github/acme/server.git/info/refs", None),
            (&get, "/github/acme/server.git/info/refs", Some("service=x")),
            (
                &post,
                "/github/acme/server.git/info/refs",
                Some("service=git-upload-pack"),
            ),
            (&get, "/github/acme/server.git/git-upload-pack", None),
            (&post, "/github/acme/../git-upload-pack", None),
            (&post, "/github/acme/server.git/objects/info/packs", None),
            (&get, "/api/v3/user", None),
            (&post, "/github/a/b/c/git-upload-pack", None),
        ] {
            assert_eq!(parse_path(method, path, query), None, "{method} {path}");
        }
    }

    #[test]
    fn basic_credentials_are_read() {
        let value = HeaderValue::from_static("Basic YzE6czNjcmV0");
        assert_eq!(
            basic_credentials(Some(&value)),
            Some(("c1".to_owned(), "s3cret".to_owned()))
        );
        assert_eq!(
            basic_credentials(Some(&HeaderValue::from_static("Bearer x"))),
            None
        );
        assert_eq!(basic_credentials(None), None);
    }
}
