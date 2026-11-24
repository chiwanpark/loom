//! The mobile core against a real agent behind an in-process relay: pair,
//! list agents, connect, prompt, approve a tool call, and catch up after
//! the link was lost.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use loom_agent::push::PushObserver;
use loom_agent::relay::{LocalHostKeys, relay_endpoint, start_relay, wait_registered};
use loom_agent::server::{ServerContext, serve_stream};
use loom_agent_core::{Agent, AgentOptions, SessionObserver, Supervisor};
use loom_client::proto::{AgentInfo, ApiError, ErrorCode, InstanceId, SessionEvent, SessionId};
use loom_client::{Client, Notification, memory_pair};
use loom_config::{
    AgentConfig, Dirs, FcmConfig, PolicyAction, PolicyConfig, PushConfig, RelayServerConfig,
};
use loom_crypto::{HostIdentity, PairingUri, Pairings};
use loom_llm::{
    AssistantContent, AssistantMessage, ChunkStream, CompletionChunk, CompletionRequest,
    LlmProvider, ProviderId, StopReason, ToolCall, Usage,
};
use loom_mobile::{
    AgentConnection, AgentUpdate, ApprovalDecision, ChangeKind, ConversationItem,
    ConversationUpdate, Delivery, DiffKind, EntryKind, Highlight, KeyModifiers, Loom, LoomError,
    Policy, PushKind, PushNotice, SessionStatus, Span, TaskEdit, TaskState, Terminal,
    TerminalFrame, TerminalKey, TerminalPlace, TerminalSummary, ToolState,
};
use tokio_util::sync::CancellationToken;

const PROVIDER: &str = "scripted";
const WAIT: Duration = Duration::from_secs(20);

struct Scripted {
    replies: Mutex<VecDeque<AssistantMessage>>,
}

fn reply(content: Vec<AssistantContent>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        provider: ProviderId::from_static(PROVIDER),
        model: "m".into(),
        content,
        usage: Usage::default(),
        stop_reason: stop,
        response_id: None,
    }
}

fn text(t: &str) -> AssistantMessage {
    reply(
        vec![AssistantContent::Text {
            text: t.into(),
            provider_data: None,
        }],
        StopReason::Stop,
    )
}

fn write_call(path: &str) -> AssistantMessage {
    reply(
        vec![AssistantContent::ToolCall(ToolCall {
            call_id: "call-1".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": path, "content": "hi"}),
            provider_data: None,
        })],
        StopReason::ToolUse,
    )
}

#[async_trait]
impl LlmProvider for Scripted {
    fn id(&self) -> ProviderId {
        ProviderId::from_static(PROVIDER)
    }

    async fn stream(&self, _request: CompletionRequest) -> loom_llm::Result<ChunkStream> {
        use futures::StreamExt;
        let message = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text("(done)"));
        let mut chunks = vec![Ok(CompletionChunk::Start { response_id: None })];
        let body = message.text();
        if !body.is_empty() {
            chunks.push(Ok(CompletionChunk::TextDelta {
                index: 0,
                delta: body,
            }));
        }
        chunks.push(Ok(CompletionChunk::Done(message)));
        Ok(futures::stream::iter(chunks).boxed())
    }
}

/// An agent serving through a relay that the test can stop and start.
struct Host {
    _dir: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
    ctx: ServerContext,
    identity: Arc<HostIdentity>,
    addr: SocketAddr,
    relay: CancellationToken,
    /// The relay, for its counters.
    relay_server: Arc<loom_relay::Relay>,
    agent_link: CancellationToken,
    /// How the relay sends notifications, if it does.
    push: Option<PushConfig>,
}

impl Host {
    async fn start(replies: Vec<AssistantMessage>) -> Self {
        Self::start_with(replies, None).await
    }

    /// A host whose relay sends notifications as `push` says.
    async fn start_with(replies: Vec<AssistantMessage>, push: Option<PushConfig>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let dirs = Dirs {
            home: root.clone(),
            config_home: root.join("config"),
            data_home: root.join("data"),
            state_home: root.join("state"),
            runtime_dir: None,
        };
        let data = root.join("data/loom");
        std::fs::create_dir_all(&data).unwrap();
        let identity = Arc::new(HostIdentity::load_or_create(&data).unwrap());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (relay, relay_server) = serve_relay(listener, &identity, push.clone());

        let mut config = AgentConfig::default();
        config.llm.default_model = Some(format!("{PROVIDER}/m"));
        config.policy = PolicyConfig {
            default: PolicyAction::Ask,
            rules: Vec::new(),
            ..PolicyConfig::default()
        };
        config.relay.enabled = true;
        config.relay.url = Some(format!("ws://{addr}"));
        let mut options = AgentOptions::new(&workspace, config.clone(), dirs);
        options.providers.push(Arc::new(Scripted {
            replies: Mutex::new(replies.into()),
        }));
        // As `loom-agent serve` does for an agent with a relay.
        let (push_observer, push_events) = PushObserver::new();
        options.observer = Some(push_observer as Arc<dyn SessionObserver>);
        let agent = Agent::new(options).unwrap();
        let info = AgentInfo {
            instance: InstanceId("phone-test".into()),
            workspace,
            pid: std::process::id(),
            started_at: 0,
            version: "test".into(),
            default_model: Some(format!("{PROVIDER}/m")),
        };
        let keys = Arc::new(LocalHostKeys::new(identity.clone(), &data, &config));
        let mut ctx = ServerContext::new(agent, info);
        ctx.host_keys = Some(keys.clone());
        ctx.supervisor = Some(Arc::new(Supervised {
            this: ctx.info.clone(),
            others: Mutex::new(Vec::new()),
        }));
        let agent_link = CancellationToken::new();
        let endpoint = relay_endpoint(&config).unwrap().unwrap();
        let mut handle = start_relay(
            ctx.clone(),
            endpoint,
            keys,
            &data,
            Some(push_events),
            agent_link.clone(),
        );
        assert!(
            wait_registered(&mut handle).await,
            "the agent did not register"
        );
        Self {
            _dir: dir,
            root,
            data,
            ctx,
            identity,
            addr,
            relay,
            relay_server,
            agent_link,
            push,
        }
    }

    /// A counter from the relay's metrics.
    fn relay_counter(&self, name: &str) -> u64 {
        self.relay_server
            .metrics_text()
            .lines()
            .find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {name} in the relay's metrics"))
    }

    fn pairing_link(&self) -> String {
        let (_, secret) = Pairings::in_dir(&self.data)
            .create(Duration::from_secs(600))
            .unwrap();
        PairingUri {
            relay_url: format!("ws://{}", self.addr),
            host: self.identity.host_id(),
            instance: self.ctx.info.instance.clone(),
            host_static: self.identity.static_public(),
            secret,
        }
        .to_string()
    }

    /// A client connected to the agent directly, as one on the host is.
    async fn local_client(&self) -> Client {
        let (client_side, server_side) = memory_pair();
        tokio::spawn(serve_stream(self.ctx.clone(), server_side));
        Client::connect(client_side, "local").await.unwrap().0
    }

    fn stop_relay(&self) {
        self.relay.cancel();
    }

    /// Starts the relay again on its old address.
    async fn restart_relay(&mut self) {
        let listener = tokio::net::TcpListener::bind(self.addr).await.unwrap();
        (self.relay, self.relay_server) = serve_relay(listener, &self.identity, self.push.clone());
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.agent_link.cancel();
        self.relay.cancel();
    }
}

/// Serves a relay with the default limits, as a deployed one has.
fn serve_relay(
    listener: tokio::net::TcpListener,
    identity: &HostIdentity,
    push: Option<PushConfig>,
) -> (CancellationToken, Arc<loom_relay::Relay>) {
    let config = RelayServerConfig {
        allowed_hosts: vec![identity.host_id().to_string()],
        push,
        ..Default::default()
    };
    let shutdown = CancellationToken::new();
    let relay = loom_relay::Relay::new(config);
    tokio::spawn(loom_relay::serve(relay.clone(), listener, shutdown.clone()));
    (shutdown, relay)
}

/// What the app keeps of the updates: the conversation and the link, kept
/// up to date the way the Kotlin side does.
#[derive(Default)]
struct App {
    connected: Option<bool>,
    items: Vec<ConversationItem>,
    keys: Vec<u64>,
    last: Option<ConversationUpdate>,
    sessions: Vec<loom_mobile::SessionSummary>,
    tasks: Option<Vec<loom_mobile::TaskItem>>,
    terminals: Vec<TerminalSummary>,
}

impl App {
    fn apply(&mut self, update: AgentUpdate) {
        match update {
            AgentUpdate::Link { connected, .. } => self.connected = Some(connected),
            AgentUpdate::Sessions { sessions } => self.sessions = sessions,
            AgentUpdate::Conversation { update } => {
                if update.reset {
                    self.items.clear();
                    self.keys.clear();
                }
                self.items.truncate(update.len as usize);
                self.keys.truncate(update.len as usize);
                for changed in &update.items {
                    let index = changed.index as usize;
                    if index < self.items.len() {
                        self.items[index] = changed.item.clone();
                        self.keys[index] = changed.key;
                    } else {
                        assert_eq!(index, self.items.len(), "a gap in {update:?}");
                        self.items.push(changed.item.clone());
                        self.keys.push(changed.key);
                    }
                }
                assert_eq!(self.items.len(), update.len as usize);
                self.last = Some(update);
            }
            AgentUpdate::TasksChanged { tasks, .. } => self.tasks = Some(tasks),
            AgentUpdate::Terminals { terminals } => self.terminals = terminals,
            AgentUpdate::NotesChanged { .. } | AgentUpdate::Notice { .. } => {}
        }
    }

    /// Takes in updates until `done` holds.
    async fn until(
        &mut self,
        conn: &Arc<AgentConnection>,
        what: &str,
        done: impl Fn(&Self) -> bool,
    ) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            let updates = tokio::time::timeout_at(deadline, conn.clone().next_updates())
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for {what}; items: {:#?}", self.items)
                })
                .expect("the connection closed");
            for update in updates {
                self.apply(update);
            }
        }
    }

    /// Whether no turn runs: the session is idle, or completed and not yet
    /// seen.
    fn idle(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|u| matches!(u.status, SessionStatus::Idle | SessionStatus::Completed))
    }

    fn status(&self) -> Option<SessionStatus> {
        self.last.as_ref().map(|u| u.status)
    }

    fn texts(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::User { text, .. } => Some(format!("user: {text}")),
                ConversationItem::Assistant { text, .. } => Some(format!("assistant: {text}")),
                ConversationItem::Tool { name, state, .. } => Some(format!("{name}: {state:?}")),
                ConversationItem::Notice { .. } => None,
            })
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_pairs_prompts_approves_and_catches_up() {
    let mut host = Host::start(vec![
        text("hello from the agent"),
        write_call("out.txt"),
        text("written"),
        text("while you were away"),
    ])
    .await;
    let phone_dir = tempfile::tempdir().unwrap();
    let loom = Loom::new(phone_dir.path().to_string_lossy().into_owned()).unwrap();

    let bad = loom
        .clone()
        .pair("loom://nope".into(), "phone".into(), "lab".into())
        .await
        .unwrap_err();
    assert!(matches!(bad, LoomError::Invalid { .. }), "{bad:?}");
    let paired = loom
        .clone()
        .pair(host.pairing_link(), "phone".into(), "lab".into())
        .await
        .unwrap();
    assert_eq!(paired.name, "lab");
    assert_eq!(paired.instance.as_deref(), Some("phone-test"));
    assert_eq!(loom.hosts(), std::slice::from_ref(&paired));
    // Pairing with the host again updates its entry.
    assert!(
        loom.clone()
            .pair(host.pairing_link(), "phone".into(), "lab".into())
            .await
            .is_ok()
    );
    assert_eq!(loom.hosts().len(), 1, "pairing again keeps one entry");

    let agents = loom.clone().list_agents("lab".into()).await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(
        agents[0].workspace.as_deref(),
        Some(host.root.join("ws").to_string_lossy().as_ref())
    );

    let conn = loom
        .clone()
        .connect("lab".into(), "phone-test".into())
        .await
        .unwrap();
    assert!(conn.is_connected());
    assert_eq!(conn.agent().version.as_deref(), Some("test"));
    let mut app = App::default();

    // A prompt streams its answer.
    let session = conn.clone().create_session(None).await.unwrap();
    assert_eq!(session.status, SessionStatus::Idle);
    conn.clone().open_session(session.id.clone()).await.unwrap();
    app.until(&conn, "the opened session", |app| app.last.is_some())
        .await;
    conn.clone()
        .send_prompt(
            session.id.clone(),
            "say hello".into(),
            Vec::new(),
            Delivery::Queue,
        )
        .await
        .unwrap();
    app.until(&conn, "the answer", |app| {
        app.idle() && app.texts().len() == 2
    })
    .await;
    assert_eq!(
        app.texts(),
        ["user: say hello", "assistant: hello from the agent"]
    );
    // The completed turn waits to be seen.
    app.until(&conn, "the completed state", |app| {
        app.status() == Some(SessionStatus::Completed)
    })
    .await;
    conn.clone().mark_seen(session.id.clone()).await.unwrap();
    app.until(&conn, "the seen session to be idle", |app| {
        app.status() == Some(SessionStatus::Idle)
    })
    .await;
    assert!(
        app.sessions
            .iter()
            .any(|s| s.id == session.id && s.title == "say hello"),
        "{:?}",
        app.sessions
    );

    // A tool call waits for approval.
    conn.clone()
        .send_prompt(
            session.id.clone(),
            "write it".into(),
            Vec::new(),
            Delivery::Queue,
        )
        .await
        .unwrap();
    app.until(&conn, "the approval", |app| {
        app.last.as_ref().is_some_and(|u| u.pending_approvals == 1)
    })
    .await;
    let call_id = app
        .items
        .iter()
        .find_map(|item| match item {
            ConversationItem::Tool {
                call_id,
                state: ToolState::WaitingApproval,
                ..
            } => Some(call_id.clone()),
            _ => None,
        })
        .unwrap();

    // A message sent meanwhile waits, and can be taken back.
    conn.clone()
        .send_prompt(
            session.id.clone(),
            "and then".into(),
            Vec::new(),
            Delivery::Steer,
        )
        .await
        .unwrap();
    app.until(&conn, "the waiting message", |app| {
        app.last.as_ref().is_some_and(|u| u.waiting.len() == 1)
    })
    .await;
    let waiting = app.last.as_ref().unwrap().waiting[0].clone();
    assert_eq!(
        (waiting.text.as_str(), waiting.delivery),
        ("and then", Delivery::Steer)
    );
    let taken = conn
        .clone()
        .take_back(session.id.clone(), waiting.id)
        .await
        .unwrap();
    assert_eq!(taken.text, "and then");
    app.until(&conn, "no waiting message", |app| {
        app.last.as_ref().is_some_and(|u| u.waiting.is_empty())
    })
    .await;

    conn.clone()
        .resolve_approval(session.id.clone(), call_id, ApprovalDecision::Allow)
        .await
        .unwrap();
    app.until(&conn, "the written file", |app| {
        app.idle() && app.texts().last().map(String::as_str) == Some("assistant: written")
    })
    .await;
    assert!(app.texts().contains(&"write: Succeeded".to_owned()));
    assert_eq!(
        std::fs::read_to_string(host.root.join("ws/out.txt")).unwrap(),
        "hi"
    );

    // Tasks come back when edited, and as a notice to every client.
    let tasks = conn
        .clone()
        .update_tasks(
            session.id.clone(),
            vec![TaskEdit::Add {
                text: "ship it".into(),
                state: TaskState::InProgress,
            }],
        )
        .await
        .unwrap();
    assert_eq!(tasks[0].text, "ship it");
    app.until(&conn, "the task notice", |app| app.tasks.is_some())
        .await;
    let notes = conn.clone().notes().await.unwrap();
    let version = conn
        .clone()
        .write_notes("remember".into(), notes.version.clone())
        .await
        .unwrap();
    assert_ne!(version, notes.version);
    let stale = conn
        .clone()
        .write_notes("again".into(), notes.version)
        .await
        .unwrap_err();
    assert!(matches!(stale, LoomError::Api { .. }), "{stale:?}");

    let prompt = conn
        .clone()
        .system_prompt(session.id.clone())
        .await
        .unwrap();
    assert!(prompt.text.starts_with("You are Loom"), "{}", prompt.text);
    assert!(
        prompt.tools.iter().any(|tool| tool.name == "read"),
        "{prompt:?}"
    );

    // The agent's default for tool calls reaches the conversation, and then the session's own.
    assert_eq!(app.last.as_ref().unwrap().policy, Policy::Ask);
    conn.clone()
        .set_policy(session.id.clone(), Policy::Allow)
        .await
        .unwrap();
    app.until(&conn, "the session's policy", |app| {
        app.last.as_ref().unwrap().policy == Policy::Allow
    })
    .await;

    // The link drops; another client runs a turn meanwhile.
    let before = app.items.len();
    host.stop_relay();
    app.until(&conn, "the lost link", |app| app.connected == Some(false))
        .await;
    assert!(!conn.is_connected());
    let err = conn
        .clone()
        .send_prompt(
            session.id.clone(),
            "lost".into(),
            Vec::new(),
            Delivery::Queue,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, LoomError::Closed), "{err:?}");
    let local = host.local_client().await;
    local
        .send_text(&SessionId(session.id.clone()), "are you there")
        .await
        .unwrap();

    // Reconnecting catches up on what was missed, without repeats.
    host.restart_relay().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while let Err(err) = conn.clone().reconnect().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "could not reconnect: {err}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    app.until(&conn, "the missed turn", |app| {
        app.idle()
            && app.texts().last().map(String::as_str) == Some("assistant: while you were away")
    })
    .await;
    assert_eq!(app.connected, Some(true));
    assert_eq!(app.items.len(), before + 2, "{:#?}", app.texts());
    assert_eq!(app.texts()[app.texts().len() - 2], "user: are you there");

    // Archiving moves the session, and the update reaches the list.
    conn.clone()
        .archive_session(session.id.clone(), true)
        .await
        .unwrap();
    app.until(&conn, "the archived session", |app| {
        app.sessions
            .iter()
            .any(|s| s.id == session.id && s.archived)
    })
    .await;

    // A cancelled wait does not take the updates of the next one.
    let waiting = tokio::spawn(conn.clone().next_updates());
    tokio::time::sleep(Duration::from_millis(100)).await;
    waiting.abort();
    let _ = waiting.await;
    conn.clone()
        .rename_session(session.id.clone(), Some("renamed".into()))
        .await
        .unwrap();
    app.until(&conn, "the renamed session", |app| {
        app.sessions.iter().any(|s| s.title == "renamed")
    })
    .await;

    // A live link answers a check.
    assert!(conn.clone().check_link(5_000).await);

    // The agent leaves the relay: a check finds the link gone, and
    // connecting again says the agent is not running.
    host.agent_link.cancel();
    let deadline = tokio::time::Instant::now() + WAIT;
    while conn.clone().check_link(1_000).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the link outlived the agent"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    app.until(&conn, "the link lost with the agent", |app| {
        app.connected == Some(false)
    })
    .await;
    assert!(!conn.is_connected());
    let err = conn.clone().reconnect().await.unwrap_err();
    assert!(matches!(err, LoomError::Offline { .. }), "{err:?}");

    conn.disconnect();
    let end = tokio::time::timeout(WAIT, conn.clone().next_updates())
        .await
        .unwrap();
    // Updates still queued come first; then the end.
    if end.is_some() {
        assert_eq!(
            tokio::time::timeout(WAIT, conn.clone().next_updates())
                .await
                .unwrap(),
            None
        );
    }
}

/// A session as long as the ones that made the relay drop the agent: the
/// phone opens it with a few relay frames, gets only its latest items, and
/// loads earlier ones a page at a time, under the relay's default limits.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_session_opens_through_the_relay_within_its_limits() {
    let host = Host::start(Vec::new()).await;
    // A client on the host runs the turns, as the desktop does.
    let (local, mut local_notes) = {
        let (client_side, server_side) = memory_pair();
        tokio::spawn(serve_stream(host.ctx.clone(), server_side));
        Client::connect(client_side, "local").await.unwrap()
    };
    let session = local.create_session(None).await.unwrap().id;
    local.subscribe(&session, None, None).await.unwrap();
    // About six events a turn: 850 turns make over 5,000.
    const TURNS: usize = 850;
    for turn in 0..TURNS {
        local
            .send_text(&session, &format!("turn {turn}"))
            .await
            .unwrap();
        'turn: loop {
            let note = tokio::time::timeout(WAIT, local_notes.recv())
                .await
                .expect("timed out waiting for a turn")
                .expect("the local connection closed");
            if let Notification::Events { events, .. } = note {
                for event in events {
                    if matches!(event.event, SessionEvent::TurnFinished { .. }) {
                        break 'turn;
                    }
                }
            }
        }
    }
    let events = local.list_sessions().await.unwrap()[0].last_seq;
    assert!(events > 5000, "only {events} events");

    let phone_dir = tempfile::tempdir().unwrap();
    let loom = Loom::new(phone_dir.path().to_string_lossy().into_owned()).unwrap();
    loom.clone()
        .pair(host.pairing_link(), "phone".into(), "lab".into())
        .await
        .unwrap();
    let conn = loom
        .clone()
        .connect("lab".into(), "phone-test".into())
        .await
        .unwrap();
    let mut app = App::default();
    let frames = host.relay_counter("loom_relay_frames_forwarded_total");
    conn.clone().open_session(session.0.clone()).await.unwrap();
    app.until(&conn, "the latest items", |app| {
        app.last.as_ref().is_some_and(|u| u.has_older) && app.items.len() >= 50
    })
    .await;
    let opened = host.relay_counter("loom_relay_frames_forwarded_total") - frames;
    assert!(
        opened < 100,
        "opening the session took {opened} relay frames"
    );
    // The latest turns, each a prompt and a reply.
    assert!(app.items.len() <= 52, "{} items", app.items.len());
    assert_eq!(
        app.texts().last().map(String::as_str),
        Some("assistant: (done)")
    );
    assert!(
        app.texts().contains(&format!("user: turn {}", TURNS - 1)),
        "{:?}",
        app.texts()
    );

    // Earlier items come a page at a time, before the ones shown, which keep
    // their keys.
    let shown = app.keys.clone();
    assert!(conn.clone().load_earlier(session.0.clone()).await.unwrap());
    app.until(&conn, "the earlier items", |app| {
        app.items.len() > shown.len()
    })
    .await;
    assert!(
        app.items.len() <= shown.len() + 52,
        "{} items",
        app.items.len()
    );
    assert_eq!(app.keys[app.keys.len() - shown.len()..], shown[..]);
    assert!(
        app.keys.windows(2).all(|w| w[0] < w[1]),
        "keys out of order"
    );

    // New events still follow the window.
    local.send_text(&session, "one more").await.unwrap();
    app.until(&conn, "the new turn", |app| {
        app.texts().iter().any(|t| t == "user: one more") && app.idle()
    })
    .await;

    assert_eq!(host.relay_counter("loom_relay_rate_limited_total"), 0);
    assert_ne!(app.connected, Some(false), "the phone lost its link");
    assert!(conn.is_connected());
}

/// A stand-in for Google's token endpoint and FCM's send API, which keeps
/// the messages it is sent and rejects the token "dead".
struct FakeFcm {
    config: PushConfig,
    messages: tokio::sync::mpsc::UnboundedReceiver<(String, String, String)>,
    _key_dir: tempfile::TempDir,
}

impl FakeFcm {
    async fn start() -> Self {
        use axum::Json;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use base64::Engine;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, messages) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .route(
                "/token",
                post(|body: String| async move {
                    assert!(body.contains("grant_type=urn"), "{body}");
                    assert!(body.contains("assertion="), "{body}");
                    Json(serde_json::json!({"access_token": "access", "expires_in": 3600}))
                }),
            )
            .route(
                "/v1/projects/loom-test/messages:send",
                post(
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let tx = tx.clone();
                        async move {
                            assert_eq!(headers["authorization"], "Bearer access");
                            let message = &body["message"];
                            assert_eq!(message["android"]["priority"], "HIGH");
                            assert!(message.get("notification").is_none(), "{body}");
                            let field = |v: &serde_json::Value| v.as_str().unwrap().to_owned();
                            let token = field(&message["token"]);
                            if token == "dead" {
                                return (
                                    StatusCode::NOT_FOUND,
                                    r#"{"error":{"status":"NOT_FOUND","details":[{"errorCode":"UNREGISTERED"}]}}"#,
                                );
                            }
                            let _ = tx.send((
                                token,
                                field(&message["data"]["h"]),
                                field(&message["data"]["p"]),
                            ));
                            (StatusCode::OK, r#"{"name":"projects/loom-test/messages/1"}"#)
                        }
                    },
                ),
            );
        tokio::spawn(async move { axum::serve(listener, app).await });

        // A service account key as Firebase hands it out, with a new RSA key.
        let key = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let der: aws_lc_rs::encoding::Pkcs8V1Der =
            aws_lc_rs::encoding::AsDer::as_der(&key).unwrap();
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            lines.join("\n")
        );
        let key_dir = tempfile::tempdir().unwrap();
        let path = key_dir.path().join("service-account.json");
        let account = serde_json::json!({
            "type": "service_account",
            "project_id": "loom-test",
            "private_key_id": "k1",
            "private_key": pem,
            "client_email": "relay@loom-test.iam.gserviceaccount.com",
            "token_uri": format!("http://{addr}/token"),
        });
        std::fs::write(&path, account.to_string()).unwrap();
        Self {
            config: PushConfig {
                fcm: Some(FcmConfig {
                    service_account: path,
                    api_url: Some(format!("http://{addr}")),
                }),
            },
            messages,
            _key_dir: key_dir,
        }
    }

    /// The next message FCM was sent: its token and data.
    async fn next(&mut self) -> (String, String, String) {
        tokio::time::timeout(WAIT, self.messages.recv())
            .await
            .expect("timed out waiting for a notification")
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_is_told_about_approvals_and_ended_turns() {
    let mut fcm = FakeFcm::start().await;
    let host = Host::start_with(
        vec![
            write_call("out.txt"),
            text("written\nand more"),
            text("again"),
        ],
        Some(fcm.config.clone()),
    )
    .await;
    let phone_dir = tempfile::tempdir().unwrap();
    let loom = Loom::new(phone_dir.path().to_string_lossy().into_owned()).unwrap();
    loom.clone()
        .pair(host.pairing_link(), "phone".into(), "lab".into())
        .await
        .unwrap();
    let conn = loom
        .clone()
        .connect("lab".into(), "phone-test".into())
        .await
        .unwrap();
    conn.clone()
        .set_push_token(Some("phone-token".into()))
        .await
        .unwrap();
    let clients = loom_crypto::AuthorizedClients::in_dir(&host.data);
    let token = || clients.list().unwrap()[0].push_token.clone();
    assert_eq!(token().as_deref(), Some("phone-token"));

    // A tool call that waits for approval is a notification.
    let mut app = App::default();
    let session = conn.clone().create_session(None).await.unwrap();
    conn.clone().open_session(session.id.clone()).await.unwrap();
    conn.clone()
        .send_prompt(
            session.id.clone(),
            "write it".into(),
            Vec::new(),
            Delivery::Queue,
        )
        .await
        .unwrap();
    let (to, host_id, payload) = fcm.next().await;
    assert_eq!(to, "phone-token");
    assert_eq!(host_id, host.identity.host_id().to_string());
    let notice = loom.open_push(host_id.clone(), payload.clone()).unwrap();
    assert_eq!(
        notice,
        PushNotice {
            host: "lab".into(),
            instance: "phone-test".into(),
            workspace: host.root.join("ws").to_string_lossy().into_owned(),
            codespace: None,
            session: session.id.clone(),
            title: notice.title.clone(),
            kind: PushKind::Approval,
            detail: Some("write: out.txt".into()),
            at: notice.at,
        }
    );
    assert!(notice.at > 0);
    // Only the paired phone opens it, and only as the host's.
    let stranger_dir = tempfile::tempdir().unwrap();
    let stranger = Loom::new(stranger_dir.path().to_string_lossy().into_owned()).unwrap();
    assert!(
        stranger
            .open_push(host_id.clone(), payload.clone())
            .is_err()
    );
    assert!(loom.open_push("ab".repeat(32), payload.clone()).is_err());
    assert!(
        loom.open_push(host_id.clone(), "not base64!".into())
            .is_err()
    );

    // A completed turn is one, with the first line of its answer.
    app.until(&conn, "the approval", |app| {
        app.last.as_ref().is_some_and(|u| u.pending_approvals == 1)
    })
    .await;
    let call_id = app
        .items
        .iter()
        .find_map(|item| match item {
            ConversationItem::Tool { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .unwrap();
    conn.clone()
        .resolve_approval(session.id.clone(), call_id, ApprovalDecision::Allow)
        .await
        .unwrap();
    let (_, host_id, payload) = fcm.next().await;
    let notice = loom.open_push(host_id, payload).unwrap();
    assert_eq!(notice.kind, PushKind::Finished);
    assert_eq!(notice.detail.as_deref(), Some("written"));
    assert_eq!(notice.title.as_deref(), Some("write it"));

    // A token FCM no longer knows is forgotten.
    conn.clone()
        .set_push_token(Some("dead".into()))
        .await
        .unwrap();
    app.until(&conn, "the end of the turn", |app| app.idle())
        .await;
    conn.clone()
        .send_prompt(
            session.id.clone(),
            "again".into(),
            Vec::new(),
            Delivery::Queue,
        )
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    while token().is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the dead token stayed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(host.relay_counter("loom_relay_pushes_rejected_total"), 1);
    assert_eq!(host.relay_counter("loom_relay_pushes_sent_total"), 2);

    // Clients on the host have no push token to set.
    let local = host.local_client().await;
    assert!(local.set_push_token(Some("t")).await.is_err());
}

/// A phone paired with `host` as "lab", and its connection to the agent.
async fn phone(host: &Host) -> (tempfile::TempDir, Arc<Loom>, Arc<AgentConnection>) {
    let dir = tempfile::tempdir().unwrap();
    let loom = Loom::new(dir.path().to_string_lossy().into_owned()).unwrap();
    loom.clone()
        .pair(host.pairing_link(), "phone".into(), "lab".into())
        .await
        .unwrap();
    let conn = loom
        .clone()
        .connect("lab".into(), "phone-test".into())
        .await
        .unwrap();
    (dir, loom, conn)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_browses_the_workspace_and_reads_highlighted_files() {
    let host = Host::start(Vec::new()).await;
    let ws = host.root.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/main.rs"), "fn main() {\n    // hi\n}\n").unwrap();
    std::fs::write(ws.join("README"), "plain").unwrap();
    std::fs::write(ws.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    let (_dir, _loom, conn) = phone(&host).await;

    let root = conn.clone().list_directory(String::new()).await.unwrap();
    assert_eq!(root.path, "");
    let names: Vec<_> = root
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert_eq!(
        names,
        [
            ("src", EntryKind::Directory),
            ("blob.bin", EntryKind::File),
            ("README", EntryKind::File)
        ]
    );
    let src = conn.clone().list_directory("src".into()).await.unwrap();
    assert_eq!(src.entries[0].path, "src/main.rs");
    assert_eq!(src.entries[0].size, 24);

    let file = conn.clone().read_file("src/main.rs".into()).await.unwrap();
    assert_eq!(file.language.as_deref(), Some("Rust"));
    let texts: Vec<_> = file.lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["fn main() {", "    // hi", "}"]);
    assert_eq!(
        file.lines[1].spans,
        [Span {
            start: 4,
            end: 9,
            highlight: Highlight::Comment
        }]
    );
    let plain = conn.clone().read_file("README".into()).await.unwrap();
    assert!(plain.language.is_none() && plain.lines[0].spans.is_empty());
    let blob = conn.clone().read_file("blob.bin".into()).await.unwrap();
    assert!(blob.binary && blob.lines.is_empty());

    let missing = conn.clone().read_file("nope.rs".into()).await.unwrap_err();
    assert!(
        matches!(&missing, LoomError::Api { code, .. } if code == "NotFound"),
        "{missing:?}"
    );
    let outside = conn.clone().list_directory("../..".into()).await;
    assert!(matches!(outside, Err(LoomError::Invalid { .. })));

    // Images and PDF files are copied whole to the phone for its viewers,
    // through the relay.
    let saved = host.root.join("saved.bin");
    let dest = saved.display().to_string();
    let size = conn
        .clone()
        .save_file("blob.bin".into(), dest.clone(), 4)
        .await
        .unwrap();
    assert_eq!(size, 4);
    assert_eq!(std::fs::read(&saved).unwrap(), [0u8, 1, 2, 3]);
    std::fs::remove_file(&saved).unwrap();
    let large = conn
        .clone()
        .save_file("blob.bin".into(), dest, 3)
        .await
        .unwrap_err();
    assert!(matches!(large, LoomError::Invalid { .. }), "{large:?}");
    assert!(!saved.exists());
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_sees_uncommitted_changes_and_what_a_session_changed() {
    let host = Host::start(vec![write_call("src/lib.rs"), text("written")]).await;
    let ws = host.root.join("ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/lib.rs"), "fn old() {}\n").unwrap();
    std::fs::write(ws.join("README.md"), "# Title\n").unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    std::fs::write(ws.join("new.py"), "print('hi')\n").unwrap();
    let (_dir, _loom, conn) = phone(&host).await;
    let mut app = App::default();

    // The session overwrites src/lib.rs.
    let session = conn.clone().create_session(None).await.unwrap();
    conn.clone().open_session(session.id.clone()).await.unwrap();
    conn.clone()
        .send_prompt(
            session.id.clone(),
            "write".into(),
            Vec::new(),
            Delivery::Queue,
        )
        .await
        .unwrap();
    app.until(&conn, "the approval", |app| {
        app.last.as_ref().is_some_and(|u| u.pending_approvals == 1)
    })
    .await;
    conn.clone()
        .resolve_approval(session.id.clone(), "call-1".into(), ApprovalDecision::Allow)
        .await
        .unwrap();
    app.until(&conn, "the end of the turn", |app| {
        app.idle() && app.texts().len() == 3
    })
    .await;

    let changes = conn.clone().git_changes().await.unwrap();
    assert!(changes.repository);
    assert_eq!(changes.branch.as_deref(), Some("main"));
    let files: Vec<_> = changes
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.kind, f.added, f.removed))
        .collect();
    assert!(
        files.contains(&("src/lib.rs", ChangeKind::Modified, 1, 1)),
        "{files:?}"
    );
    assert!(
        files
            .iter()
            .any(|f| f.0 == "new.py" && f.1 == ChangeKind::Untracked),
        "{files:?}"
    );

    let diff = conn
        .clone()
        .git_diff("src/lib.rs".into(), None)
        .await
        .unwrap();
    assert_eq!((diff.added, diff.removed), (1, 1));
    assert_eq!(diff.language.as_deref(), Some("Rust"));
    let rows: Vec<_> = diff
        .rows
        .iter()
        .filter(|r| r.kind != DiffKind::Hunk)
        .map(|r| (r.kind, r.text.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (DiffKind::Removed, "fn old() {}"),
            (DiffKind::Added, "hi"),
            (DiffKind::Note, "\\ No newline at end of file")
        ]
    );
    let untracked = conn.clone().git_diff("new.py".into(), None).await.unwrap();
    assert_eq!(untracked.added, 1);
    assert!(
        untracked.rows.iter().any(|r| r.kind == DiffKind::Added
            && r.spans.iter().any(|s| s.highlight == Highlight::String))
    );

    let touched = conn
        .clone()
        .session_changes(session.id.clone())
        .await
        .unwrap();
    assert_eq!(touched.len(), 1, "{touched:?}");
    assert_eq!(touched[0].display_path, "src/lib.rs");
    assert!(!touched[0].created);
    let diff = conn
        .clone()
        .session_diff(session.id.clone(), touched[0].path.clone())
        .await
        .unwrap();
    assert_eq!(diff.path, "src/lib.rs");
    assert!(
        diff.rows
            .iter()
            .any(|r| r.kind == DiffKind::Added && r.text == "hi")
    );
    assert!(
        conn.clone()
            .session_diff(session.id.clone(), "/elsewhere".into())
            .await
            .is_err()
    );
}

/// A frame's text, a line per row, as the app would draw it.
fn screen(frame: &TerminalFrame) -> String {
    frame
        .lines
        .iter()
        .map(|runs| {
            let mut line = String::new();
            for run in runs {
                while line.chars().count() < run.column as usize {
                    line.push(' ');
                }
                line.push_str(&run.text);
            }
            line.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Takes frames until one satisfies `done`.
async fn frame_until(
    terminal: &Arc<Terminal>,
    what: &str,
    done: impl Fn(&TerminalFrame) -> bool,
) -> TerminalFrame {
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut last = None;
    loop {
        let frame = tokio::time::timeout_at(deadline, terminal.clone().next_frame())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for {what}; screen:\n{}",
                    last.as_ref().map(screen).unwrap_or_default()
                )
            })
            .expect("the terminal closed");
        if done(&frame) {
            return frame;
        }
        last = Some(frame);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_runs_a_terminal_that_survives_a_lost_link() {
    let mut host = Host::start(Vec::new()).await;
    let (_dir, _loom, conn) = phone(&host).await;

    let terminal = conn.clone().open_terminal(40, 10).await.unwrap();
    terminal.send_text("echo one-$((1+2))".into());
    terminal.send_key(TerminalKey::Enter, KeyModifiers::default());
    let frame = frame_until(&terminal, "the echo", |f| screen(f).contains("\none-3")).await;
    assert_eq!((frame.cols, frame.rows), (40, 10));
    assert!(frame.cursor.is_some());

    // Resizing reaches the shell.
    terminal.resize(50, 12);
    terminal.send_text("echo cols=$(tput cols)\r".into());
    frame_until(&terminal, "the new size", |f| {
        f.cols == 50 && screen(f).contains("cols=50")
    })
    .await;

    // A lost link leaves the shell running; the new link takes it over.
    host.stop_relay();
    let mut app = App::default();
    app.until(&conn, "the lost link", |app| app.connected == Some(false))
        .await;
    host.restart_relay().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while let Err(err) = conn.clone().reconnect().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "could not reconnect: {err}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    terminal.send_text("echo two-$((2+2))".into());
    terminal.send_key(TerminalKey::Enter, KeyModifiers::default());
    frame_until(&terminal, "the echo after reconnecting", |f| {
        screen(f).contains("two-4")
    })
    .await;
    assert!(terminal.text().contains("two-4"));

    // Ctrl-D ends the shell.
    terminal.send_key(
        TerminalKey::Char { text: "d".into() },
        KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
    );
    let frame = frame_until(&terminal, "the exit", |f| f.exit_status.is_some()).await;
    assert_eq!(frame.exit_status, Some(0));
    terminal.end();
    assert!(terminal.clone().next_frame().await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn terminals_move_between_the_phone_and_the_desktop() {
    let host = Host::start(Vec::new()).await;
    let (_dir, _loom, conn) = phone(&host).await;
    let desktop = host.local_client().await;
    let id = desktop.open_terminal(80, 24, 1).await.unwrap();
    desktop
        .terminal_input(id, b"echo desk-$((2+3))\n".to_vec())
        .await
        .unwrap();
    let place = |app: &App| {
        app.terminals
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.place.clone())
    };
    let mut app = App::default();
    app.until(&conn, "the desktop's terminal", |app| {
        place(app)
            == Some(TerminalPlace::Elsewhere {
                client: "local".into(),
            })
    })
    .await;

    // The phone takes it over, with what it printed.
    let terminal = conn.clone().attach_terminal(id, 40, 10).await.unwrap();
    assert!(terminal.is_here());
    frame_until(&terminal, "the desktop's output", |f| {
        screen(f).contains("desk-5")
    })
    .await;
    app.until(&conn, "the terminal here", |app| {
        place(app) == Some(TerminalPlace::Here)
    })
    .await;
    terminal.send_text("echo phone-$((3+4)); stty size\r".into());
    frame_until(&terminal, "the phone's input", |f| {
        screen(f).contains("phone-7") && screen(f).contains("10 40")
    })
    .await;
    assert!(desktop.terminal_input(id, b"x".to_vec()).await.is_err());

    // The desktop takes it back; the phone keeps its screen and stops typing.
    desktop.attach_terminal(id, 80, 24, 1, true).await.unwrap();
    app.until(&conn, "the terminal on the desktop", |app| {
        matches!(place(app), Some(TerminalPlace::Elsewhere { .. }))
    })
    .await;
    assert!(!terminal.is_here());
    assert!(terminal.text().contains("phone-7"));

    // And the phone takes it once more.
    terminal.clone().take_over().await.unwrap();
    assert!(terminal.is_here());
    frame_until(&terminal, "the replay", |f| {
        screen(f).contains("phone-7") && f.ended.is_none()
    })
    .await;
    app.until(&conn, "the terminal back here", |app| {
        place(app) == Some(TerminalPlace::Here)
    })
    .await;

    // When the desktop closes it after taking it, the phone's ends.
    desktop.attach_terminal(id, 80, 24, 1, true).await.unwrap();
    desktop.close_terminal(id).await.unwrap();
    let frame = frame_until(&terminal, "the end", |f| f.ended.is_some()).await;
    assert_eq!(
        frame.ended.as_deref(),
        Some("The terminal was closed on another client.")
    );
    app.until(&conn, "no terminals", |app| app.terminals.is_empty())
        .await;
}

/// Stands in for `loom-agent serve`'s supervisor: the agent's own workspace
/// has it, any other existing directory gets a (pretend) new agent, and the
/// rest is refused as the real one refuses it.
struct Supervised {
    this: AgentInfo,
    others: Mutex<Vec<AgentInfo>>,
}

#[async_trait]
impl Supervisor for Supervised {
    async fn list(&self) -> Result<Vec<AgentInfo>, ApiError> {
        let mut all = vec![self.this.clone()];
        all.extend(self.others.lock().unwrap().iter().cloned());
        Ok(all)
    }

    async fn spawn(&self, workspace: PathBuf) -> Result<AgentInfo, ApiError> {
        if workspace == self.this.workspace {
            return Ok(self.this.clone());
        }
        if !workspace.is_dir() {
            return Err(ApiError::new(
                ErrorCode::NotFound,
                format!("{} is not a directory", workspace.display()),
            ));
        }
        let info = AgentInfo {
            instance: InstanceId(format!("spawned-{}", self.others.lock().unwrap().len())),
            workspace,
            ..self.this.clone()
        };
        self.others.lock().unwrap().push(info.clone());
        Ok(info)
    }

    async fn stop(&self, _instance: &InstanceId) -> Result<(), ApiError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_starts_an_agent_for_another_directory() {
    let host = Host::start(Vec::new()).await;
    let (_dir, loom, conn) = phone(&host).await;
    let other = host.root.join("other");
    std::fs::create_dir_all(&other).unwrap();

    // The agent's own workspace has its agent already.
    let same = conn
        .clone()
        .spawn_agent(host.root.join("ws").display().to_string())
        .await
        .unwrap();
    assert_eq!(same.instance, "phone-test");

    let spawned = conn
        .clone()
        .spawn_agent(format!("  {}  ", other.display()))
        .await
        .unwrap();
    assert_eq!(spawned.instance, "spawned-0");
    assert_eq!(spawned.workspace.as_deref(), Some(other.to_str().unwrap()));
    // The host's agents list it.
    let agents = loom.clone().list_agents("lab".into()).await.unwrap();
    assert!(
        agents.iter().any(|a| a.instance == "spawned-0"),
        "{agents:?}"
    );

    let missing = conn
        .clone()
        .spawn_agent(host.root.join("nope").display().to_string())
        .await
        .unwrap_err();
    assert!(
        matches!(&missing, LoomError::Api { code, .. } if code == "NotFound"),
        "{missing:?}"
    );
    assert!(matches!(
        conn.clone().spawn_agent("  ".into()).await,
        Err(LoomError::Invalid { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_updates_the_agents_of_a_host_without_a_link() {
    let host = Host::start(Vec::new()).await;
    let (_dir, loom, _conn) = phone(&host).await;
    let err = loom
        .clone()
        .update_agents("lab".into(), Some("0.5.0".into()))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LoomError::Api { code, message } if code == "Unsupported" && message.contains("cannot update itself")),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_forwards_a_port_of_the_agents_host_through_the_relay() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let host = Host::start(Vec::new()).await;
    let (_dir, _loom, conn) = phone(&host).await;
    // A server on the agent's host that sends a page larger than a tunnel's
    // window for any request.
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    let page: Vec<u8> = (0..1_500_000).map(|i| (i % 253) as u8).collect();
    let served = page.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = server.accept().await.unwrap();
            let page = served.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 4];
                stream.read_exact(&mut request).await.unwrap();
                stream.write_all(&page).await.unwrap();
            });
        }
    });

    let ports = conn.clone().list_ports().await.unwrap();
    let listed = ports
        .iter()
        .find(|p| p.port == port)
        .expect("the port is listed");
    assert_eq!(listed.host, "127.0.0.1");
    let forward = conn
        .clone()
        .forward_port(listed.host.clone(), port, None)
        .await
        .unwrap();
    assert_eq!(
        (forward.host().as_str(), forward.port()),
        ("127.0.0.1", port)
    );

    for _ in 0..2 {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", forward.local_port()))
            .await
            .unwrap();
        stream.write_all(b"GET!").await.unwrap();
        let mut received = vec![0u8; page.len()];
        tokio::time::timeout(Duration::from_secs(30), stream.read_exact(&mut received))
            .await
            .expect("the page arrived")
            .unwrap();
        assert!(received == page);
    }

    // A forward on the same local port, as after the agent restarted.
    let local = forward.local_port();
    forward.stop();
    assert_eq!(forward.connections(), 0);
    let again = conn
        .clone()
        .forward_port(listed.host.clone(), port, Some(local))
        .await
        .unwrap();
    assert_eq!(again.local_port(), local);
    again.stop();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", forward.local_port()))
            .await
            .is_err()
    );
}
