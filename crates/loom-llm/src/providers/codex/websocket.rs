//! Responses over a WebSocket, as the Codex CLI talks to the ChatGPT backend.
//!
//! Each session keeps its socket between requests. When the next request only
//! adds to the history of the last one, it sends just the new input items with
//! `previous_response_id`, and the server keeps the rest from the socket.
//! Any failure before the response starts sends the request over HTTP instead.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{Map, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::auth::ORIGINATOR;
use crate::error::{LlmError, Result};
use crate::provider::ChunkStream;
use crate::responses::http::events_to_chunks;
use crate::responses::request::history_input;
use crate::sse::SseEvent;
use crate::types::{CompletionRequest, Message as ChatMessage};

pub(super) const BETA: &str = "responses_websockets=2026-02-06";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest wait for the next event of a response.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// The server ends a socket after 60 minutes; start a new one a bit before.
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// How long an unused socket is kept for its session.
const KEEP_IDLE: Duration = Duration::from_secs(10 * 60);
const MAX_IDLE: usize = 32;
/// Large requests go out as one message split into frames of this size.
const FRAGMENT: usize = 1024 * 1024;
/// Events read while waiting for the response to start.
const MAX_PRELUDE: usize = 16;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) struct Websockets {
    url: String,
    enabled: AtomicBool,
    idle: Mutex<HashMap<String, Idle>>,
}

struct Conn {
    socket: Socket,
    opened: Instant,
    token: String,
    last: Option<Last>,
}

/// What the server holds for the last response on a socket.
struct Last {
    /// The request body without `input`.
    base: Value,
    /// The full input of the request.
    input: Vec<Value>,
    /// How many history messages the request had; the reply comes next.
    messages: usize,
    response_id: String,
}

struct Idle {
    conn: Conn,
    since: Instant,
}

/// The `ws(s)://` form of the `http(s)://` responses URL.
pub(super) fn websocket_url(responses_url: &str) -> String {
    if let Some(rest) = responses_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = responses_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        responses_url.to_owned()
    }
}

impl Websockets {
    pub(super) fn new(responses_url: &str, enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            url: websocket_url(responses_url),
            enabled: AtomicBool::new(enabled),
            idle: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Starts `request` on the session's socket. An error means the response
    /// did not start and the request should go over HTTP.
    pub(super) async fn start(
        self: &Arc<Self>,
        provider: &str,
        session: &str,
        auth: (&str, &str),
        request: &CompletionRequest,
        body: &Value,
    ) -> std::result::Result<ChunkStream, String> {
        let mut base = body
            .as_object()
            .cloned()
            .ok_or("the body is not an object")?;
        let Some(Value::Array(input)) = base.remove("input") else {
            return Err("the body has no input".into());
        };
        let base = Value::Object(base);
        let pending = |base: &Value, input: &[Value]| Last {
            base: base.clone(),
            input: input.to_vec(),
            messages: request.messages.len(),
            response_id: String::new(),
        };

        if let Some(mut conn) = self.take(session, auth.0) {
            let (items, previous) =
                match continuation(provider, conn.last.as_ref(), &base, &input, request) {
                    Some((items, previous)) => (items, Some(previous)),
                    None => (input.clone(), None),
                };
            conn.last = None;
            tracing::debug!(provider, continues = ?previous, items = items.len(), "reusing the Responses socket");
            match begin(&mut conn.socket, create(&base, items, previous.as_deref())).await {
                Ok(prelude) => {
                    let last = pending(&base, &input);
                    return Ok(self.stream(provider, request, session, conn, prelude, last));
                }
                Err(err) => {
                    tracing::debug!(provider, error = %err, "reopening the Responses socket")
                }
            }
        }

        let mut conn = self.connect(session, auth).await?;
        let last = pending(&base, &input);
        let prelude = begin(&mut conn.socket, create(&base, input, None)).await?;
        Ok(self.stream(provider, request, session, conn, prelude, last))
    }

    fn take(&self, session: &str, token: &str) -> Option<Conn> {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.retain(|_, entry| {
            entry.since.elapsed() < KEEP_IDLE && entry.conn.opened.elapsed() < MAX_AGE
        });
        idle.remove(session)
            .map(|entry| entry.conn)
            .filter(|conn| conn.token == token)
    }

    fn put(&self, session: String, conn: Conn) {
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        if idle.len() >= MAX_IDLE && !idle.contains_key(&session) {
            let oldest = idle
                .iter()
                .min_by_key(|(_, entry)| entry.since)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                idle.remove(&oldest);
            }
        }
        idle.insert(
            session,
            Idle {
                conn,
                since: Instant::now(),
            },
        );
    }

    async fn connect(
        &self,
        session: &str,
        (token, account_id): (&str, &str),
    ) -> std::result::Result<Conn, String> {
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("{}: {e}", self.url))?;
        let headers = request.headers_mut();
        let mut set = |name: &'static str, value: &str| {
            HeaderValue::from_str(value)
                .map(|value| {
                    headers.insert(name, value);
                })
                .map_err(|_| format!("the {name} header has invalid characters"))
        };
        set("authorization", &format!("Bearer {token}"))?;
        set("chatgpt-account-id", account_id)?;
        set("originator", ORIGINATOR)?;
        set("openai-beta", BETA)?;
        set("session-id", session)?;
        set("x-client-request-id", session)?;

        let connected = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(request, None, true),
        )
        .await
        .map_err(|_| format!("{}: timed out", self.url))?;
        match connected {
            Ok((socket, _)) => Ok(Conn {
                socket,
                opened: Instant::now(),
                token: token.to_owned(),
                last: None,
            }),
            Err(WsError::Http(response)) => {
                let status = response.status().as_u16();
                // Auth, rate limits and outages fail over HTTP too; anything
                // else means this endpoint does not take sockets.
                if !matches!(status, 401 | 408 | 429) && status < 500 {
                    self.enabled.store(false, Ordering::Relaxed);
                    tracing::info!(url = %self.url, status, "Responses sockets are unavailable; using HTTP");
                }
                Err(format!("{}: the server answered {status}", self.url))
            }
            Err(err) => Err(format!("{}: {err}", self.url)),
        }
    }

    fn stream(
        self: &Arc<Self>,
        provider: &str,
        request: &CompletionRequest,
        session: &str,
        conn: Conn,
        prelude: VecDeque<String>,
        last: Last,
    ) -> ChunkStream {
        let state = EventState {
            owner: Arc::clone(self),
            session: session.to_owned(),
            provider: provider.to_owned(),
            conn: Some(conn),
            prelude,
            last: Some(last),
            response_id: None,
        };
        let events = futures::stream::unfold(state, |mut st| async move {
            let event = st.next().await?;
            Some((event, st))
        });
        events_to_chunks(provider, &request.model, Box::pin(events)).boxed()
    }
}

struct EventState {
    owner: Arc<Websockets>,
    session: String,
    provider: String,
    conn: Option<Conn>,
    prelude: VecDeque<String>,
    /// What to remember once the response completes.
    last: Option<Last>,
    response_id: Option<String>,
}

impl EventState {
    async fn next(&mut self) -> Option<Result<SseEvent>> {
        let text = match self.prelude.pop_front() {
            Some(text) => text,
            None => {
                let conn = self.conn.as_mut()?;
                match read_text(&mut conn.socket).await {
                    Ok(text) => text,
                    Err(message) => {
                        self.conn = None;
                        return Some(Err(LlmError::Stream {
                            provider: self.provider.clone(),
                            code: None,
                            message,
                        }));
                    }
                }
            }
        };
        let value: Option<Value> = serde_json::from_str(&text).ok();
        let kind = value
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str);
        let response_id = value
            .as_ref()
            .and_then(|v| v.get("response"))
            .and_then(|r| r.get("id"))
            .and_then(Value::as_str);
        if let Some(id) = response_id {
            self.response_id = Some(id.to_owned());
        }
        match kind {
            Some("response.completed" | "response.incomplete" | "response.done") => {
                // The socket is free again; keep it for the session's next request.
                if let (Some(mut conn), Some(mut last), Some(id)) =
                    (self.conn.take(), self.last.take(), self.response_id.clone())
                {
                    last.response_id = id;
                    conn.last = Some(last);
                    self.owner.put(self.session.clone(), conn);
                }
            }
            Some("response.failed" | "error") => self.conn = None,
            _ => {}
        }
        Some(Ok(SseEvent {
            event: None,
            data: text,
        }))
    }
}

/// The new input items and the response they follow, when `request` only
/// adds to the history the socket's last response already holds.
fn continuation(
    provider: &str,
    last: Option<&Last>,
    base: &Value,
    input: &[Value],
    request: &CompletionRequest,
) -> Option<(Vec<Value>, String)> {
    let last = last?;
    if last.base != *base || !input.starts_with(&last.input) {
        return None;
    }
    let Some(ChatMessage::Assistant(reply)) = request.messages.get(last.messages) else {
        return None;
    };
    if reply.response_id.as_deref() != Some(last.response_id.as_str()) {
        return None;
    }
    let known = history_input(
        provider,
        &request.model,
        &request.messages[..=last.messages],
    )
    .len();
    if known < last.input.len() || known >= input.len() {
        return None;
    }
    Some((input[known..].to_vec(), last.response_id.clone()))
}

fn create(base: &Value, input: Vec<Value>, previous: Option<&str>) -> String {
    let mut message = Map::new();
    message.insert("type".into(), "response.create".into());
    if let Value::Object(fields) = base {
        message.extend(fields.clone());
    }
    if let Some(previous) = previous {
        message.insert("previous_response_id".into(), previous.into());
    }
    message.insert("input".into(), Value::Array(input));
    Value::Object(message).to_string()
}

/// Sends the request and reads until the response starts. The events read
/// are returned so the stream can replay them.
async fn begin(
    socket: &mut Socket,
    message: String,
) -> std::result::Result<VecDeque<String>, String> {
    tokio::time::timeout(IDLE_TIMEOUT, send_text(socket, message))
        .await
        .map_err(|_| "timed out sending the request".to_owned())?
        .map_err(|e| format!("send failed: {e}"))?;
    let mut prelude = VecDeque::new();
    loop {
        let text = read_text(socket).await?;
        let kind = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        if kind == "error" || kind == "response.failed" {
            return Err(format!("the server refused the request: {text}"));
        }
        let started = kind.starts_with("response.");
        prelude.push_back(text);
        if started || prelude.len() >= MAX_PRELUDE {
            return Ok(prelude);
        }
    }
}

async fn send_text(socket: &mut Socket, text: String) -> std::result::Result<(), WsError> {
    if text.len() <= FRAGMENT {
        return socket.send(Message::Text(text.into())).await;
    }
    let mut data = bytes::Bytes::from(text);
    let mut opcode = OpCode::Data(Data::Text);
    while !data.is_empty() {
        let payload = data.split_to(data.len().min(FRAGMENT));
        let frame = Frame::message(payload, opcode, data.is_empty());
        socket.feed(Message::Frame(frame)).await?;
        opcode = OpCode::Data(Data::Continue);
    }
    socket.flush().await
}

async fn read_text(socket: &mut Socket) -> std::result::Result<String, String> {
    loop {
        let message = tokio::time::timeout(IDLE_TIMEOUT, socket.next())
            .await
            .map_err(|_| "timed out waiting for the server".to_owned())?;
        match message {
            Some(Ok(Message::Text(text))) => return Ok(text.to_string()),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Binary(_))) => return Err("the server sent binary data".into()),
            Some(Ok(Message::Close(frame))) => {
                let reason = frame
                    .map(|f| format!(" ({} {})", f.code, f.reason))
                    .unwrap_or_default();
                return Err(format!("the server closed the socket{reason}"));
            }
            Some(Err(err)) => return Err(err.to_string()),
            None => return Err("the socket closed".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantContent, AssistantMessage, StopReason, Usage};
    use serde_json::json;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::User {
            content: vec![crate::types::InputContent::Text { text: text.into() }],
        }
    }

    fn reply(id: &str) -> ChatMessage {
        ChatMessage::Assistant(AssistantMessage {
            provider: crate::provider::ProviderId::new("openai-codex"),
            model: "m".into(),
            content: vec![AssistantContent::text("hi")],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            response_id: Some(id.into()),
        })
    }

    fn request(messages: Vec<ChatMessage>) -> CompletionRequest {
        CompletionRequest {
            model: "m".into(),
            messages,
            ..Default::default()
        }
    }

    fn input(req: &CompletionRequest) -> Vec<Value> {
        history_input("openai-codex", &req.model, &req.messages)
    }

    #[test]
    fn urls_switch_scheme() {
        assert_eq!(
            websocket_url("https://x/codex/responses"),
            "wss://x/codex/responses"
        );
        assert_eq!(websocket_url("http://127.0.0.1:9/r"), "ws://127.0.0.1:9/r");
    }

    #[test]
    fn continuation_sends_only_new_items() {
        let first = request(vec![user("a")]);
        let base = json!({"model": "m"});
        let last = Last {
            base: base.clone(),
            input: input(&first),
            messages: 1,
            response_id: "resp_1".into(),
        };
        let next = request(vec![user("a"), reply("resp_1"), user("b")]);
        let (items, previous) =
            continuation("openai-codex", Some(&last), &base, &input(&next), &next).unwrap();
        assert_eq!(previous, "resp_1");
        assert_eq!(items, input(&request(vec![user("b")])));

        // Anything else sends the whole history.
        let other = request(vec![user("a"), reply("resp_9"), user("b")]);
        assert!(continuation("openai-codex", Some(&last), &base, &input(&other), &other).is_none());
        let edited = request(vec![user("x"), reply("resp_1"), user("b")]);
        assert!(
            continuation("openai-codex", Some(&last), &base, &input(&edited), &edited).is_none()
        );
        let changed = json!({"model": "n"});
        assert!(
            continuation("openai-codex", Some(&last), &changed, &input(&next), &next).is_none()
        );
        let nothing_new = request(vec![user("a"), reply("resp_1")]);
        assert!(
            continuation(
                "openai-codex",
                Some(&last),
                &base,
                &input(&nothing_new),
                &nothing_new
            )
            .is_none()
        );
    }
}
