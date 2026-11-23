use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

use crate::{Image, ImageAlias, Incus, Instance, Project};

pub type ExecHandler = Box<dyn Fn(&str, &[String]) -> (i32, String, String) + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeFile {
    pub content: Vec<u8>,
    pub kind: String,
    pub uid: u32,
    pub gid: u32,
    pub mode: String,
}

#[derive(Debug, Clone, Default)]
pub struct FakeInstance {
    pub instance: Instance,
    pub source: Value,
    pub files: BTreeMap<String, FakeFile>,
    pub execs: Vec<Vec<String>>,
}

pub struct FakeState {
    pub instances: BTreeMap<String, FakeInstance>,
    pub images: Vec<Image>,
    pub projects: BTreeMap<String, Project>,
    pub pools: BTreeMap<String, String>,
    pub networks: Vec<String>,
    pub requests: Vec<String>,
    pub certificates: Vec<String>,
    pub reject_zstd: bool,
    pub exec: Option<ExecHandler>,
    operations: BTreeMap<String, Value>,
    logs: BTreeMap<String, Vec<u8>>,
    next: u64,
}

impl Default for FakeState {
    fn default() -> Self {
        Self {
            instances: BTreeMap::new(),
            images: Vec::new(),
            projects: BTreeMap::new(),
            pools: BTreeMap::from([("default".to_owned(), "dir".to_owned())]),
            networks: vec!["incusbr0".to_owned()],
            requests: Vec::new(),
            certificates: Vec::new(),
            reject_zstd: false,
            exec: None,
            operations: BTreeMap::new(),
            logs: BTreeMap::new(),
            next: 1,
        }
    }
}

pub struct FakeIncus {
    pub socket: PathBuf,
    state: Arc<Mutex<FakeState>>,
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(high) = hex(bytes[i + 1])
            && let Some(low) = hex(bytes[i + 2])
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sync(metadata: Value) -> (StatusCode, Bytes) {
    let body =
        json!({"type": "sync", "status": "Success", "status_code": 200, "metadata": metadata});
    (StatusCode::OK, Bytes::from(body.to_string()))
}

fn error(status: StatusCode, message: &str) -> (StatusCode, Bytes) {
    let body = json!({"type": "error", "error": message, "error_code": status.as_u16()});
    (status, Bytes::from(body.to_string()))
}

impl FakeState {
    fn operation(&mut self, result: Result<Value, String>) -> (StatusCode, Bytes) {
        let id = format!("op-{}", self.next);
        self.next += 1;
        let operation = match result {
            Ok(metadata) => {
                json!({"id": id, "status": "Success", "status_code": 200, "metadata": metadata, "err": ""})
            }
            Err(err) => {
                json!({"id": id, "status": "Failure", "status_code": 400, "metadata": null, "err": err})
            }
        };
        self.operations.insert(id.clone(), operation);
        let body = json!({
            "type": "async",
            "status": "Operation created",
            "status_code": 100,
            "operation": format!("/1.0/operations/{id}"),
            "metadata": {"id": id, "status": "Running", "status_code": 103},
        });
        (StatusCode::ACCEPTED, Bytes::from(body.to_string()))
    }

    fn image_by_alias(&self, alias: &str) -> Option<&Image> {
        self.images
            .iter()
            .find(|image| image.aliases.iter().any(|a| a.name == alias))
    }

    fn handle(
        &mut self,
        method: &Method,
        path: &str,
        query: &BTreeMap<String, String>,
        headers: &hyper::HeaderMap,
        body: &[u8],
    ) -> (StatusCode, Bytes) {
        let json_body = || serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method.clone(), parts.as_slice()) {
            (Method::GET, ["1.0"]) => sync(json!({
                "auth": "trusted",
                "api_extensions": ["instances", "image_compression_algorithm"],
                "environment": {
                    "server_version": "6.0",
                    "server_name": "fake",
                    "kernel_architecture": std::env::consts::ARCH,
                    "storage": "dir",
                    "storage_version": "1",
                    "project": query.get("project").cloned().unwrap_or_else(|| "default".into()),
                }
            })),
            (Method::GET, ["1.0", "resources"]) => sync(json!({
                "cpu": {"architecture": std::env::consts::ARCH, "total": 8},
                "memory": {"total": 16u64 << 30},
            })),
            (Method::GET, ["1.0", "storage-pools", name]) => match self.pools.get(*name) {
                Some(driver) => sync(json!({"name": name, "driver": driver, "status": "Created"})),
                None => error(StatusCode::NOT_FOUND, "Storage pool not found"),
            },
            (Method::GET, ["1.0", "networks", name]) => {
                if self.networks.iter().any(|n| n == name) {
                    sync(json!({"name": name, "type": "bridge", "managed": true}))
                } else {
                    error(StatusCode::NOT_FOUND, "Network not found")
                }
            }
            (Method::GET, ["1.0", "projects", name]) => match self.projects.get(*name) {
                Some(project) => sync(serde_json::to_value(project).unwrap_or_default()),
                None => error(StatusCode::NOT_FOUND, "Project not found"),
            },
            (Method::POST, ["1.0", "projects"]) => {
                let project: Project = serde_json::from_value(json_body()).unwrap_or_default();
                if self.projects.contains_key(&project.name) {
                    return error(StatusCode::CONFLICT, "Project already exists");
                }
                self.projects.insert(project.name.clone(), project);
                sync(Value::Null)
            }
            (Method::POST, ["1.0", "certificates"]) => {
                let token = json_body()["trust_token"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.certificates.push(token);
                sync(Value::Null)
            }
            (Method::GET, ["1.0", "instances"]) => sync(
                self.instances
                    .values()
                    .map(|i| serde_json::to_value(&i.instance).unwrap_or_default())
                    .collect(),
            ),
            (Method::POST, ["1.0", "instances"]) => {
                let request = json_body();
                let name = request["name"].as_str().unwrap_or_default().to_owned();
                if self.instances.contains_key(&name) {
                    return self.operation(Err("Instance already exists".into()));
                }
                let source = request["source"].clone();
                let local = source.get("server").is_none();
                if local
                    && let Some(alias) = source["alias"].as_str()
                    && self.image_by_alias(alias).is_none()
                {
                    return self.operation(Err(format!("Image {alias} not found")));
                }
                let instance = Instance {
                    name: name.clone(),
                    status: "Stopped".into(),
                    kind: request["type"].as_str().unwrap_or("container").into(),
                    config: serde_json::from_value(request["config"].clone()).unwrap_or_default(),
                    devices: serde_json::from_value(request["devices"].clone()).unwrap_or_default(),
                    profiles: serde_json::from_value(request["profiles"].clone())
                        .unwrap_or_default(),
                };
                self.instances.insert(
                    name,
                    FakeInstance {
                        instance,
                        source,
                        ..FakeInstance::default()
                    },
                );
                self.operation(Ok(Value::Null))
            }
            (method, ["1.0", "instances", name, rest @ ..]) => {
                let name = name.to_string();
                if !self.instances.contains_key(&name) {
                    return error(StatusCode::NOT_FOUND, "Instance not found");
                }
                self.instance(method, &name, rest, query, headers, body)
            }
            (Method::GET, ["1.0", "images"]) => {
                sync(serde_json::to_value(&self.images).unwrap_or_default())
            }
            (Method::GET, ["1.0", "images", "aliases", name]) => match self.image_by_alias(name) {
                Some(image) => sync(json!({"name": name, "target": image.fingerprint})),
                None => error(StatusCode::NOT_FOUND, "Image alias not found"),
            },
            (Method::POST, ["1.0", "images"]) => {
                let request = json_body();
                if self.reject_zstd && request["compression_algorithm"] == "zstd" {
                    return self.operation(Err(
                        "Compression algorithm \"zstd\" isn't supported on this system".into(),
                    ));
                }
                let source = request["source"]["name"].as_str().unwrap_or_default();
                match self.instances.get(source) {
                    Some(instance) if instance.instance.running() => {
                        return self.operation(Err("The instance is running".into()));
                    }
                    None => return self.operation(Err("Instance not found".into())),
                    _ => {}
                }
                let fingerprint = format!("{:064x}", self.next);
                let aliases: Vec<ImageAlias> =
                    serde_json::from_value(request["aliases"].clone()).unwrap_or_default();
                for alias in &aliases {
                    for image in &mut self.images {
                        image.aliases.retain(|a| a.name != alias.name);
                    }
                }
                self.images.push(Image {
                    fingerprint: fingerprint.clone(),
                    aliases,
                    properties: serde_json::from_value(request["properties"].clone())
                        .unwrap_or_default(),
                    size: 1 << 20,
                    created_at: String::new(),
                });
                self.operation(Ok(json!({"fingerprint": fingerprint})))
            }
            (Method::DELETE, ["1.0", "images", fingerprint]) => {
                let before = self.images.len();
                self.images
                    .retain(|image| image.fingerprint != *fingerprint);
                if self.images.len() == before {
                    return error(StatusCode::NOT_FOUND, "Image not found");
                }
                self.operation(Ok(Value::Null))
            }
            (Method::GET, ["1.0", "operations", id, "wait"]) => match self.operations.get(*id) {
                Some(operation) => sync(operation.clone()),
                None => error(StatusCode::NOT_FOUND, "Operation not found"),
            },
            _ => error(StatusCode::NOT_FOUND, "not found"),
        }
    }

    fn instance(
        &mut self,
        method: Method,
        name: &str,
        rest: &[&str],
        query: &BTreeMap<String, String>,
        headers: &hyper::HeaderMap,
        body: &[u8],
    ) -> (StatusCode, Bytes) {
        let header = |key: &str| {
            headers
                .get(key)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        match (method, rest) {
            (Method::GET, []) => {
                sync(serde_json::to_value(&self.instances[name].instance).unwrap_or_default())
            }
            (Method::DELETE, []) => {
                if self.instances[name].instance.running() {
                    return self.operation(Err("Instance is running".into()));
                }
                self.instances.remove(name);
                self.operation(Ok(Value::Null))
            }
            (Method::GET, ["state"]) => {
                let running = self.instances[name].instance.running();
                let network = if running {
                    json!({"eth0": {"addresses": [{"family": "inet", "address": "10.1.2.3", "scope": "global"}]}})
                } else {
                    json!({})
                };
                sync(json!({"status": self.instances[name].instance.status, "network": network}))
            }
            (Method::PUT, ["state"]) => {
                let request = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
                let status = match request["action"].as_str() {
                    Some("start" | "restart") => "Running",
                    Some("stop") => "Stopped",
                    _ => return self.operation(Err("bad action".into())),
                };
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.instance.status = status.into();
                }
                self.operation(Ok(Value::Null))
            }
            (Method::POST, ["files"]) => {
                let Some(path) = query.get("path") else {
                    return error(StatusCode::BAD_REQUEST, "no path");
                };
                let file = FakeFile {
                    content: body.to_vec(),
                    kind: header("x-incus-type"),
                    uid: header("x-incus-uid").parse().unwrap_or_default(),
                    gid: header("x-incus-gid").parse().unwrap_or_default(),
                    mode: header("x-incus-mode"),
                };
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.files.insert(path.clone(), file);
                }
                sync(Value::Null)
            }
            (Method::POST, ["exec"]) => {
                let request = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
                let command: Vec<String> =
                    serde_json::from_value(request["command"].clone()).unwrap_or_default();
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.execs.push(command.clone());
                }
                if !self.instances[name].instance.running() {
                    return self.operation(Err("Instance is not running".into()));
                }
                let (code, stdout, stderr) = match &self.exec {
                    Some(handler) => handler(name, &command),
                    None => (0, String::new(), String::new()),
                };
                let id = self.next;
                let base = format!("/1.0/instances/{name}/logs/exec-output/exec_{id}");
                self.logs
                    .insert(format!("{base}.stdout"), stdout.into_bytes());
                self.logs
                    .insert(format!("{base}.stderr"), stderr.into_bytes());
                self.operation(Ok(json!({
                    "return": code,
                    "output": {"1": format!("{base}.stdout"), "2": format!("{base}.stderr")},
                })))
            }
            (method, ["logs", "exec-output", file]) => {
                let key = format!("/1.0/instances/{name}/logs/exec-output/{file}");
                match method {
                    Method::GET => match self.logs.get(&key) {
                        Some(bytes) => (StatusCode::OK, Bytes::from(bytes.clone())),
                        None => error(StatusCode::NOT_FOUND, "log not found"),
                    },
                    Method::DELETE => {
                        self.logs.remove(&key);
                        sync(Value::Null)
                    }
                    _ => error(StatusCode::BAD_REQUEST, "bad method"),
                }
            }
            _ => error(StatusCode::NOT_FOUND, "not found"),
        }
    }
}

async fn respond(
    state: Arc<Mutex<FakeState>>,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query: BTreeMap<String, String> = request
        .uri()
        .query()
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((decode(key), decode(value)))
        })
        .collect();
    let headers = request.headers().clone();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    let (status, bytes) = {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        let line = match request_query(&query) {
            Some(query) => format!("{method} {path}?{query}"),
            None => format!("{method} {path}"),
        };
        state.requests.push(line);
        state.handle(&method, &path, &query, &headers, &body)
    };
    Response::builder()
        .status(status)
        .body(Full::new(bytes))
        .unwrap_or_default()
}

fn request_query(query: &BTreeMap<String, String>) -> Option<String> {
    let shown: Vec<String> = query
        .iter()
        .filter(|(key, _)| key.as_str() != "project" && key.as_str() != "timeout")
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    (!shown.is_empty()).then(|| shown.join("&"))
}

impl FakeIncus {
    pub async fn start(dir: &Path) -> Self {
        let socket = dir.join("incus.socket");
        let listener =
            tokio::net::UnixListener::bind(&socket).expect("cannot bind the fake socket");
        let state = Arc::new(Mutex::new(FakeState::default()));
        let shared = state.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(respond(state, request).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self { socket, state }
    }

    pub fn client(&self, project: Option<&str>) -> Incus {
        Incus::unix(&self.socket).with_project(project.map(str::to_owned))
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_image(&self, alias: &str, properties: BTreeMap<String, String>) -> String {
        let mut state = self.state();
        let fingerprint = format!("{:064x}", 1000 + state.images.len());
        state.images.push(Image {
            fingerprint: fingerprint.clone(),
            aliases: vec![ImageAlias {
                name: alias.to_owned(),
                description: String::new(),
            }],
            properties,
            size: 1 << 20,
            created_at: String::new(),
        });
        fingerprint
    }
}
