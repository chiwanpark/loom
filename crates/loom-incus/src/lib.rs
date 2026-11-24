mod stream;
mod tls;
mod types;

#[cfg(feature = "fake")]
pub mod fake;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

pub use tls::{
    GeneratedCertificate, ServerTrust, TrustToken, certificate_pem, client_config, fingerprint,
    generate_certificate, read_certificate,
};
pub use types::*;

pub const SOCKET_PATHS: &[&str] = &["/var/lib/incus/unix.socket", "/run/incus/unix.socket"];
pub const SOCKET_ENV: &str = "INCUS_SOCKET";
pub const DEFAULT_PORT: u16 = 8443;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const WAIT_STEP: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum IncusError {
    #[error("cannot connect to Incus at {target}: {source}")]
    Connect {
        target: String,
        #[source]
        source: std::io::Error,
    },
    #[error("TLS: {0}")]
    Tls(String),
    #[error("HTTP: {0}")]
    Http(String),
    #[error("Incus: {message} ({status})")]
    Api { status: u16, message: String },
    #[error("Incus operation {status}: {message}")]
    Operation { status: String, message: String },
    #[error("{0}")]
    Protocol(String),
    #[error("{0} took too long")]
    Timeout(String),
}

impl IncusError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status: 404, .. })
    }

    pub fn message(&self) -> String {
        match self {
            Self::Api { message, .. } | Self::Operation { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }
}

pub type Result<T, E = IncusError> = std::result::Result<T, E>;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[derive(Clone)]
enum Transport {
    Unix(PathBuf),
    Tls {
        host: String,
        port: u16,
        name: ServerName<'static>,
        connector: TlsConnector,
    },
}

#[derive(Clone)]
pub struct Incus {
    transport: Arc<Transport>,
    project: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_code: u16,
    #[serde(default)]
    metadata: Value,
}

pub fn find_socket() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(SOCKET_ENV) {
        return Some(PathBuf::from(path));
    }
    SOCKET_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

pub fn parse_url(url: &str) -> Result<(String, u16)> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| IncusError::Protocol(format!("{url:?} is not an https:// URL")))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or_else(|| IncusError::Protocol(format!("{url:?} has a bad IPv6 address")))?;
        (host.to_owned(), after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty() {
        return Err(IncusError::Protocol(format!("{url:?} has no host")));
    }
    let port = match port {
        Some(port) => port
            .parse()
            .map_err(|_| IncusError::Protocol(format!("{url:?} has a bad port")))?,
        None => DEFAULT_PORT,
    };
    Ok((host, port))
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn exchange<S>(stream: S, request: Request<Full<Bytes>>) -> Result<(StatusCode, Bytes)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let http = |err: hyper::Error| IncusError::Http(err.to_string());
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(http)?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let response = sender.send_request(request).await.map_err(http)?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(http)?
        .to_bytes();
    task.abort();
    Ok((status, body))
}

impl Incus {
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self {
            transport: Arc::new(Transport::Unix(path.into())),
            project: None,
        }
    }

    pub fn https(url: &str, config: rustls::ClientConfig) -> Result<Self> {
        let (host, port) = parse_url(url)?;
        let name = ServerName::try_from(host.clone())
            .map_err(|err| IncusError::Protocol(format!("{host:?} is not a host name: {err}")))?;
        Ok(Self {
            transport: Arc::new(Transport::Tls {
                host,
                port,
                name,
                connector: TlsConnector::from(Arc::new(config)),
            }),
            project: None,
        })
    }

    pub fn with_project(mut self, project: Option<String>) -> Self {
        self.project = project.filter(|project| !project.is_empty());
        self
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn target(&self) -> String {
        match &*self.transport {
            Transport::Unix(path) => path.display().to_string(),
            Transport::Tls { host, port, .. } => format!("https://{host}:{port}"),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(&*self.transport, Transport::Unix(_))
    }

    async fn connect(&self) -> Result<Box<dyn Io>> {
        match &*self.transport {
            Transport::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|source| IncusError::Connect {
                        target: path.display().to_string(),
                        source,
                    })?;
                Ok(Box::new(stream))
            }
            Transport::Tls {
                host,
                port,
                name,
                connector,
            } => {
                let target = format!("{host}:{port}");
                let tcp = tokio::net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .map_err(|source| IncusError::Connect {
                        target: target.clone(),
                        source,
                    })?;
                let stream = connector
                    .connect(name.clone(), tcp)
                    .await
                    .map_err(|err| IncusError::Tls(format!("{target}: {err}")))?;
                Ok(Box::new(stream))
            }
        }
    }

    fn authority(&self) -> String {
        match &*self.transport {
            Transport::Unix(_) => "incus".to_owned(),
            Transport::Tls { host, port, .. } => format!("{host}:{port}"),
        }
    }

    fn uri(&self, path: &str, project: bool) -> String {
        match (&self.project, project) {
            (Some(name), true) => {
                let separator = if path.contains('?') { '&' } else { '?' };
                format!("{path}{separator}project={}", encode(name))
            }
            _ => path.to_owned(),
        }
    }

    async fn raw(
        &self,
        method: Method,
        uri: &str,
        body: Bytes,
        headers: &[(&str, String)],
        timeout: Duration,
    ) -> Result<(StatusCode, Bytes)> {
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(uri)
            .header(hyper::header::HOST, self.authority())
            .header(hyper::header::USER_AGENT, "loom-incus");
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let request = builder
            .body(Full::new(body))
            .map_err(|err| IncusError::Http(err.to_string()))?;
        let what = format!("{method} {uri}");
        let send = async { exchange(self.connect().await?, request).await };
        tokio::time::timeout(timeout, send)
            .await
            .map_err(|_| IncusError::Timeout(what))?
    }

    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Envelope> {
        self.call_with(method, path, body, true).await
    }

    async fn call_with(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        project: bool,
    ) -> Result<Envelope> {
        let (bytes, headers) = match body {
            Some(body) => (
                Bytes::from(
                    serde_json::to_vec(body).map_err(|e| IncusError::Protocol(e.to_string()))?,
                ),
                vec![("content-type", "application/json".to_owned())],
            ),
            None => (Bytes::new(), Vec::new()),
        };
        let uri = self.uri(path, project);
        let (status, body) = self
            .raw(method, &uri, bytes, &headers, REQUEST_TIMEOUT)
            .await?;
        let envelope: Envelope = serde_json::from_slice(&body).map_err(|_| IncusError::Api {
            status: status.as_u16(),
            message: format!(
                "unexpected answer to {path}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        })?;
        if envelope.kind == "error" || !status.is_success() {
            let code = if envelope.error_code != 0 {
                envelope.error_code
            } else {
                status.as_u16()
            };
            let message = if envelope.error.is_empty() {
                format!("{path} failed")
            } else {
                envelope.error
            };
            return Err(IncusError::Api {
                status: code,
                message,
            });
        }
        Ok(envelope)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let envelope = self.call(Method::GET, path, None).await?;
        serde_json::from_value(envelope.metadata)
            .map_err(|err| IncusError::Protocol(format!("cannot read {path}: {err}")))
    }

    async fn run(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Operation> {
        let envelope = self.call(method, path, body).await?;
        if envelope.kind != "async" {
            return Ok(Operation {
                status: "Success".into(),
                status_code: 200,
                metadata: Some(envelope.metadata),
                ..Operation::default()
            });
        }
        let operation: Operation = serde_json::from_value(envelope.metadata)
            .map_err(|err| IncusError::Protocol(format!("cannot read the operation: {err}")))?;
        let done = self.wait(&operation.id, timeout).await?;
        if !done.succeeded() {
            return Err(IncusError::Operation {
                status: done.status,
                message: if done.err.is_empty() {
                    format!("{path} failed")
                } else {
                    done.err
                },
            });
        }
        Ok(done)
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<Operation> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return Err(IncusError::Timeout(format!("the operation {id}")));
            }
            let step = left.min(WAIT_STEP).as_secs().max(1);
            let path = format!("/1.0/operations/{}/wait?timeout={step}", encode(id));
            let uri = self.uri(&path, true);
            let (status, body) = self
                .raw(
                    Method::GET,
                    &uri,
                    Bytes::new(),
                    &[],
                    Duration::from_secs(step + 30),
                )
                .await?;
            let envelope: Envelope = serde_json::from_slice(&body).map_err(|_| {
                IncusError::Protocol(format!("unexpected answer to the wait for {id}"))
            })?;
            if envelope.kind == "error" || !status.is_success() {
                return Err(IncusError::Api {
                    status: status.as_u16(),
                    message: envelope.error,
                });
            }
            let operation: Operation = serde_json::from_value(envelope.metadata)
                .map_err(|err| IncusError::Protocol(format!("cannot read the operation: {err}")))?;
            if !operation.running() {
                return Ok(operation);
            }
        }
    }

    pub async fn server(&self) -> Result<Server> {
        self.get("/1.0").await
    }

    pub async fn resources(&self) -> Result<Resources> {
        self.get("/1.0/resources").await
    }

    pub async fn storage_pool(&self, name: &str) -> Result<StoragePool> {
        self.get(&format!("/1.0/storage-pools/{}", encode(name)))
            .await
    }

    pub async fn network(&self, name: &str) -> Result<Network> {
        self.get(&format!("/1.0/networks/{}", encode(name))).await
    }

    pub async fn project_info(&self, name: &str) -> Result<Project> {
        let envelope = self
            .call_with(
                Method::GET,
                &format!("/1.0/projects/{}", encode(name)),
                None,
                false,
            )
            .await?;
        serde_json::from_value(envelope.metadata)
            .map_err(|err| IncusError::Protocol(format!("cannot read the project: {err}")))
    }

    pub async fn create_project(&self, project: &Project) -> Result<()> {
        let body =
            serde_json::to_value(project).map_err(|e| IncusError::Protocol(e.to_string()))?;
        self.call_with(Method::POST, "/1.0/projects", Some(&body), false)
            .await?;
        Ok(())
    }

    pub async fn instances(&self) -> Result<Vec<Instance>> {
        self.get("/1.0/instances?recursion=1").await
    }

    pub async fn instance(&self, name: &str) -> Result<Option<Instance>> {
        match self.get(&format!("/1.0/instances/{}", encode(name))).await {
            Ok(instance) => Ok(Some(instance)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn instance_state(&self, name: &str) -> Result<InstanceState> {
        self.get(&format!("/1.0/instances/{}/state", encode(name)))
            .await
    }

    pub async fn create_instance(&self, body: &Value, timeout: Duration) -> Result<()> {
        self.run(Method::POST, "/1.0/instances", Some(body), timeout)
            .await?;
        Ok(())
    }

    pub async fn set_state(&self, name: &str, action: StateAction, force: bool) -> Result<()> {
        let body = json!({"action": action.as_str(), "timeout": 30, "force": force});
        self.run(
            Method::PUT,
            &format!("/1.0/instances/{}/state", encode(name)),
            Some(&body),
            Duration::from_secs(120),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_instance(&self, name: &str) -> Result<()> {
        self.run(
            Method::DELETE,
            &format!("/1.0/instances/{}", encode(name)),
            None,
            Duration::from_secs(300),
        )
        .await?;
        Ok(())
    }

    async fn push(
        &self,
        instance: &str,
        path: &str,
        body: Bytes,
        kind: &str,
        owner: FileOwner,
    ) -> Result<()> {
        let uri = self.uri(
            &format!(
                "/1.0/instances/{}/files?path={}",
                encode(instance),
                encode(path)
            ),
            true,
        );
        let headers = [
            ("x-incus-uid", owner.uid.to_string()),
            ("x-incus-gid", owner.gid.to_string()),
            ("x-incus-mode", format!("{:04o}", owner.mode)),
            ("x-incus-type", kind.to_owned()),
            ("x-incus-write", "overwrite".to_owned()),
            ("content-type", "application/octet-stream".to_owned()),
        ];
        let timeout = REQUEST_TIMEOUT + Duration::from_secs(body.len() as u64 / (1 << 20));
        let (status, answer) = self
            .raw(Method::POST, &uri, body, &headers, timeout)
            .await?;
        if status.is_success() {
            return Ok(());
        }
        let message = serde_json::from_slice::<Envelope>(&answer)
            .map(|envelope| envelope.error)
            .unwrap_or_default();
        Err(IncusError::Api {
            status: status.as_u16(),
            message: format!("cannot write {path} in {instance}: {message}"),
        })
    }

    pub async fn push_file(
        &self,
        instance: &str,
        path: &str,
        content: impl Into<Bytes>,
        owner: FileOwner,
    ) -> Result<()> {
        self.push(instance, path, content.into(), "file", owner)
            .await
    }

    pub async fn push_directory(&self, instance: &str, path: &str, owner: FileOwner) -> Result<()> {
        self.push(instance, path, Bytes::new(), "directory", owner)
            .await
    }

    pub async fn push_symlink(&self, instance: &str, path: &str, target: &str) -> Result<()> {
        self.push(
            instance,
            path,
            Bytes::from(target.to_owned()),
            "symlink",
            FileOwner::root(0o777),
        )
        .await
    }

    pub async fn exec(&self, instance: &str, exec: &Exec, timeout: Duration) -> Result<ExecOutput> {
        let mut body = json!({
            "command": exec.command,
            "environment": exec.environment,
            "interactive": false,
            "wait-for-websocket": false,
            "record-output": true,
            "user": exec.user,
            "group": exec.group,
        });
        if let Some(cwd) = &exec.cwd {
            body["cwd"] = Value::String(cwd.clone());
        }
        let path = format!("/1.0/instances/{}/exec", encode(instance));
        let envelope = self.call(Method::POST, &path, Some(&body)).await?;
        let operation: Operation = serde_json::from_value(envelope.metadata)
            .map_err(|err| IncusError::Protocol(format!("cannot read the operation: {err}")))?;
        let done = self.wait(&operation.id, timeout).await?;
        let metadata = done.metadata.clone().unwrap_or(Value::Null);
        let mut output = ExecOutput {
            code: metadata
                .get("return")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .unwrap_or(-1),
            ..ExecOutput::default()
        };
        if let Some(logs) = metadata.get("output").and_then(Value::as_object) {
            for (fd, log) in logs {
                let Some(log) = log.as_str() else { continue };
                let uri = self.uri(log, true);
                let (status, bytes) = self
                    .raw(Method::GET, &uri, Bytes::new(), &[], REQUEST_TIMEOUT)
                    .await?;
                if status.is_success() {
                    match fd.as_str() {
                        "1" => output.stdout = bytes.to_vec(),
                        "2" => output.stderr = bytes.to_vec(),
                        _ => {}
                    }
                }
                let _ = self
                    .raw(Method::DELETE, &uri, Bytes::new(), &[], REQUEST_TIMEOUT)
                    .await;
            }
        }
        if !done.succeeded() && output.code < 0 {
            return Err(IncusError::Operation {
                status: done.status,
                message: done.err,
            });
        }
        Ok(output)
    }

    pub async fn images(&self) -> Result<Vec<Image>> {
        self.get("/1.0/images?recursion=1").await
    }

    pub async fn image_alias(&self, name: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Alias {
            target: String,
        }
        match self
            .get::<Alias>(&format!("/1.0/images/aliases/{}", encode(name)))
            .await
        {
            Ok(alias) => Ok(Some(alias.target)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn publish(
        &self,
        instance: &str,
        alias: &str,
        properties: &BTreeMap<String, String>,
        compression: Option<&str>,
        timeout: Duration,
    ) -> Result<String> {
        let mut body = json!({
            "source": {"type": "instance", "name": instance},
            "aliases": [{"name": alias, "description": ""}],
            "properties": properties,
            "public": false,
        });
        if let Some(compression) = compression {
            body["compression_algorithm"] = Value::String(compression.to_owned());
        }
        let done = self
            .run(Method::POST, "/1.0/images", Some(&body), timeout)
            .await?;
        done.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("fingerprint"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| IncusError::Protocol("the new image has no fingerprint".into()))
    }

    pub async fn delete_image(&self, fingerprint: &str) -> Result<()> {
        self.run(
            Method::DELETE,
            &format!("/1.0/images/{}", encode(fingerprint)),
            None,
            Duration::from_secs(300),
        )
        .await?;
        Ok(())
    }

    pub async fn add_certificate(&self, token: &str) -> Result<()> {
        let body = json!({"type": "client", "trust_token": token.trim()});
        self.call_with(Method::POST, "/1.0/certificates", Some(&body), false)
            .await?;
        Ok(())
    }

    pub async fn update_project(
        &self,
        name: &str,
        config: &BTreeMap<String, String>,
    ) -> Result<()> {
        let body = json!({ "config": config });
        self.call_with(
            Method::PATCH,
            &format!("/1.0/projects/{}", encode(name)),
            Some(&body),
            false,
        )
        .await?;
        Ok(())
    }

    fn volume_path(pool: &str, name: &str) -> String {
        format!(
            "/1.0/storage-pools/{}/volumes/custom/{}",
            encode(pool),
            encode(name)
        )
    }

    pub async fn storage_volume(&self, pool: &str, name: &str) -> Result<Option<StorageVolume>> {
        match self.get(&Self::volume_path(pool, name)).await {
            Ok(volume) => Ok(Some(volume)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn storage_volume_state(
        &self,
        pool: &str,
        name: &str,
    ) -> Result<Option<StorageVolumeState>> {
        match self
            .get(&format!("{}/state", Self::volume_path(pool, name)))
            .await
        {
            Ok(state) => Ok(Some(state)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn create_storage_volume(
        &self,
        pool: &str,
        name: &str,
        config: &BTreeMap<String, String>,
    ) -> Result<()> {
        let body = json!({
            "name": name,
            "type": "custom",
            "content_type": "filesystem",
            "config": config,
        });
        self.run(
            Method::POST,
            &format!("/1.0/storage-pools/{}/volumes/custom", encode(pool)),
            Some(&body),
            Duration::from_secs(300),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_storage_volume(&self, pool: &str, name: &str) -> Result<()> {
        self.run(
            Method::DELETE,
            &Self::volume_path(pool, name),
            None,
            Duration::from_secs(300),
        )
        .await?;
        Ok(())
    }
}

pub async fn server_certificate(url: &str) -> Result<Vec<u8>> {
    let (host, port) = parse_url(url)?;
    let name = ServerName::try_from(host.clone())
        .map_err(|err| IncusError::Protocol(format!("{host:?} is not a host name: {err}")))?;
    let (config, seen) = tls::capture_config()?;
    let target = format!("{host}:{port}");
    let connect = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|source| IncusError::Connect {
                target: target.clone(),
                source,
            })?;
        TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|err| IncusError::Tls(format!("{target}: {err}")))
    };
    tokio::time::timeout(Duration::from_secs(30), connect)
        .await
        .map_err(|_| IncusError::Timeout(format!("the connection to {target}")))??;
    let der = seen.lock().unwrap_or_else(|e| e.into_inner()).take();
    der.ok_or_else(|| IncusError::Tls(format!("{target} sent no certificate")))
}

pub fn load_https(
    url: &str,
    certificate: &Path,
    key: &Path,
    server_certificate: Option<&Path>,
) -> Result<Incus> {
    let trust = match server_certificate {
        Some(path) => ServerTrust::Pinned(read_certificate(path)?),
        None => ServerTrust::System,
    };
    Incus::https(url, client_config(certificate, key, trust)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_have_a_host_and_a_port() {
        assert_eq!(
            parse_url("https://10.0.0.1:8443").unwrap(),
            ("10.0.0.1".to_owned(), 8443)
        );
        assert_eq!(
            parse_url("https://incus.example.com/").unwrap(),
            ("incus.example.com".to_owned(), DEFAULT_PORT)
        );
        assert_eq!(
            parse_url("https://[fd42::1]:9443").unwrap(),
            ("fd42::1".to_owned(), 9443)
        );
        for bad in ["http://x", "https://", "https://x:port"] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_and_projects_are_encoded() {
        assert_eq!(encode("/home/dev/a b.json"), "/home/dev/a%20b.json");
        assert_eq!(encode("x&y=z"), "x%26y%3Dz");
        let incus = Incus::unix("/x").with_project(Some("loom".into()));
        assert_eq!(
            incus.uri("/1.0/instances", true),
            "/1.0/instances?project=loom"
        );
        assert_eq!(
            incus.uri("/1.0/instances?recursion=1", true),
            "/1.0/instances?recursion=1&project=loom"
        );
        assert_eq!(incus.uri("/1.0/projects", false), "/1.0/projects");
        assert_eq!(
            Incus::unix("/x")
                .with_project(Some(String::new()))
                .project(),
            None
        );
    }
}
