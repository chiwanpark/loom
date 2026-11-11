mod common;

use std::sync::{Arc, Mutex};

use common::{CODEX, Harness, codex_access_token, now, sse_body, text_response_events};
use futures::{SinkExt, StreamExt};
use loom_llm::{CompletionRequest, LlmError, LlmProvider, Message, collect};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// What the server does with one `response.create`, given the socket's
/// number and the message's number on that socket.
type Script = dyn Fn(usize, usize) -> Reply + Send + Sync;

enum Reply {
    Events(Vec<Value>),
    /// Sends these events, then closes the socket.
    Close(Vec<Value>),
}

#[derive(Default)]
struct Log {
    handshakes: Vec<String>,
    /// (socket, message) for each `response.create`.
    messages: Vec<(usize, Value)>,
    posts: Vec<Value>,
}

struct Server {
    base_url: String,
    log: Arc<Mutex<Log>>,
}

impl Server {
    /// Serves sockets on `/backend-api/codex/responses` and SSE for POSTs
    /// there. `refuse` answers socket handshakes with 426.
    async fn start(refuse: bool, script: Arc<Script>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/backend-api", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Log::default()));
        let shared = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(
                    stream,
                    refuse,
                    Arc::clone(&script),
                    Arc::clone(&shared),
                ));
            }
        });
        Self { base_url, log }
    }

    fn log<T>(&self, read: impl FnOnce(&Log) -> T) -> T {
        read(&self.log.lock().unwrap())
    }
}

async fn serve(mut stream: TcpStream, refuse: bool, script: Arc<Script>, log: Arc<Mutex<Log>>) {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8];
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let header = |name: &str| {
        head.lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default()
    };
    if head.starts_with("POST") {
        let length: usize = header("content-length").parse().unwrap();
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        log.lock()
            .unwrap()
            .posts
            .push(serde_json::from_slice(&body).unwrap());
        let body = sse_body(&text_response_events("over http"));
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
        return;
    }
    let socket_index = {
        let mut log = log.lock().unwrap();
        log.handshakes.push(head.clone());
        log.handshakes.len() - 1
    };
    if refuse {
        let reply =
            "HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        stream.write_all(reply.as_bytes()).await.unwrap();
        return;
    }
    let accept = derive_accept_key(header("sec-websocket-key").as_bytes());
    let reply = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
    );
    stream.write_all(reply.as_bytes()).await.unwrap();
    let mut socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    let mut index = 0;
    while let Some(Ok(message)) = socket.next().await {
        let WsMessage::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        log.lock().unwrap().messages.push((socket_index, value));
        let (events, close) = match script(socket_index, index) {
            Reply::Events(events) => (events, false),
            Reply::Close(events) => (events, true),
        };
        for event in events {
            socket
                .send(WsMessage::Text(event.to_string().into()))
                .await
                .unwrap();
        }
        if close {
            let _ = socket.close(None).await;
            return;
        }
        index += 1;
    }
}

fn response(id: &str, text: &str) -> Vec<Value> {
    text_response_events(text)
        .into_iter()
        .map(|mut event| {
            if let Some(response) = event.get_mut("response") {
                response["id"] = json!(id);
            }
            event
        })
        .collect()
}

async fn harness() -> Harness {
    let h = Harness::new().await;
    h.set_section(
        CODEX,
        json!({
            "account_id": "acct-1",
            "access_token": codex_access_token("acct-1"),
            "refresh_token": "r",
            "expires_at": now() + 3600,
            "saved_at": now(),
        }),
    )
    .await;
    h
}

fn provider(h: &Harness, server: &Server) -> Arc<dyn LlmProvider> {
    h.provider(CODEX, json!({ "base_url": server.base_url }))
}

fn request(messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        model: "gpt-test".into(),
        system: Some("system prompt".into()),
        messages,
        session_id: Some("session-1".into()),
        ..CompletionRequest::default()
    }
}

/// Runs one turn, then a follow-up after the reply.
async fn two_turns(provider: &dyn LlmProvider) -> (Vec<Message>, String) {
    let mut messages = vec![Message::user_text("hello")];
    let first = collect(provider.stream(request(messages.clone())).await.unwrap())
        .await
        .unwrap();
    messages.push(Message::Assistant(first));
    messages.push(Message::user_text("again"));
    let second = collect(provider.stream(request(messages.clone())).await.unwrap())
        .await
        .unwrap();
    (messages, second.text())
}

#[tokio::test]
async fn follow_ups_reuse_the_socket_and_send_only_new_items() {
    let server = Server::start(
        false,
        Arc::new(|_, index| Reply::Events(response(&format!("resp_{index}"), "hi"))),
    )
    .await;
    let h = harness().await;
    let (_, text) = two_turns(provider(&h, &server).as_ref()).await;
    assert_eq!(text, "hi");

    let (handshakes, messages, posts) = server.log(|log| {
        (
            log.handshakes.clone(),
            log.messages.clone(),
            log.posts.len(),
        )
    });
    assert_eq!(handshakes.len(), 1, "one socket for both turns");
    assert_eq!(posts, 0);
    let head = handshakes[0].to_lowercase();
    assert!(head.starts_with("get /backend-api/codex/responses "));
    for expected in [
        "openai-beta: responses_websockets=2026-02-06",
        "chatgpt-account-id: acct-1",
        "originator: loom",
        "session-id: session-1",
        "authorization: bearer ",
    ] {
        assert!(head.contains(expected), "missing {expected} in {head}");
    }

    let (_, first) = &messages[0];
    assert_eq!(first["type"], "response.create");
    assert_eq!(first["instructions"], "system prompt");
    assert_eq!(first["input"].as_array().unwrap().len(), 1);
    assert!(first.get("previous_response_id").is_none());

    let (socket, second) = &messages[1];
    assert_eq!(*socket, 0);
    assert_eq!(second["previous_response_id"], "resp_0");
    let input = second["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "only the new user message: {input:?}");
    assert_eq!(input[0]["content"][0]["text"], "again");
}

#[tokio::test]
async fn a_forgotten_response_resends_the_history_on_a_new_socket() {
    let server = Server::start(
        false,
        Arc::new(|socket, index| match (socket, index) {
            (0, 1) => Reply::Events(vec![json!({
                "type": "error",
                "status": 400,
                "error": {"type": "invalid_request_error", "code": "previous_response_not_found", "message": "gone"},
            })]),
            _ => Reply::Events(response(&format!("resp_{socket}_{index}"), "hi")),
        }),
    )
    .await;
    let h = harness().await;
    let (_, text) = two_turns(provider(&h, &server).as_ref()).await;
    assert_eq!(text, "hi");

    let messages = server.log(|log| log.messages.clone());
    assert_eq!(messages.len(), 3);
    let (socket, retry) = &messages[2];
    assert_eq!(*socket, 1, "the retry uses a new socket");
    assert!(retry.get("previous_response_id").is_none());
    assert_eq!(retry["input"].as_array().unwrap().len(), 3);
    assert_eq!(server.log(|log| log.posts.len()), 0);
}

#[tokio::test]
async fn refused_sockets_fall_back_to_http_for_good() {
    let server = Server::start(true, Arc::new(|_, _| Reply::Events(vec![]))).await;
    let h = harness().await;
    let (_, text) = two_turns(provider(&h, &server).as_ref()).await;
    assert_eq!(text, "over http");
    assert_eq!(server.log(|log| log.handshakes.len()), 1, "no second try");
    assert_eq!(server.log(|log| log.posts.len()), 2);
}

#[tokio::test]
async fn an_error_before_the_response_starts_falls_back_to_http() {
    let server = Server::start(
        false,
        Arc::new(|_, _| {
            // The socket stays open: the error alone must end the attempt.
            Reply::Events(vec![json!({
                "type": "error",
                "status": 503,
                "error": {"type": "server_error", "message": "busy"},
            })])
        }),
    )
    .await;
    let h = harness().await;
    let turn = async {
        collect(
            provider(&h, &server)
                .stream(request(vec![Message::user_text("hello")]))
                .await
                .unwrap(),
        )
        .await
        .unwrap()
    };
    let message = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the error should end the socket attempt");
    assert_eq!(message.text(), "over http");
    assert_eq!(server.log(|log| log.posts.len()), 1);
}

#[tokio::test]
async fn a_socket_closed_mid_response_is_an_error() {
    let server = Server::start(
        false,
        Arc::new(|_, _| Reply::Close(response("resp_1", "hi")[..2].to_vec())),
    )
    .await;
    let h = harness().await;
    let result = collect(
        provider(&h, &server)
            .stream(request(vec![Message::user_text("hello")]))
            .await
            .unwrap(),
    )
    .await;
    assert!(matches!(result, Err(LlmError::Stream { .. })), "{result:?}");
    assert_eq!(server.log(|log| log.posts.len()), 0, "no second generation");
}

#[tokio::test]
async fn large_requests_arrive_whole() {
    let server = Server::start(
        false,
        Arc::new(|_, _| Reply::Events(response("resp_1", "hi"))),
    )
    .await;
    let h = harness().await;
    let big = "x".repeat(3 * 1024 * 1024 + 7);
    collect(
        provider(&h, &server)
            .stream(request(vec![Message::user_text(big.clone())]))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let messages = server.log(|log| log.messages.clone());
    assert_eq!(messages[0].1["input"][0]["content"][0]["text"], big);
}

#[tokio::test]
async fn http_is_used_without_a_session_or_when_turned_off() {
    let server = Server::start(
        false,
        Arc::new(|_, _| Reply::Events(response("resp_1", "hi"))),
    )
    .await;
    let h = harness().await;
    let mut no_session = request(vec![Message::user_text("hello")]);
    no_session.session_id = None;
    let message = collect(provider(&h, &server).stream(no_session).await.unwrap())
        .await
        .unwrap();
    assert_eq!(message.text(), "over http");

    let off = h.provider(
        CODEX,
        json!({ "base_url": server.base_url, "websockets": false }),
    );
    let message = collect(
        off.stream(request(vec![Message::user_text("hello")]))
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(message.text(), "over http");
    assert_eq!(server.log(|log| log.handshakes.len()), 0);
}
