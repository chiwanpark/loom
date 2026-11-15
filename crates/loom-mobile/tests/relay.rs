//! The mobile core against a real agent behind an in-process relay: pair,
//! list agents, connect, prompt, approve a tool call, and catch up after
//! the link was lost.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use loom_agent::relay::{LocalHostKeys, relay_endpoint, start_relay, wait_registered};
use loom_agent::server::{ServerContext, serve_stream};
use loom_agent_core::{Agent, AgentOptions};
use loom_client::proto::{AgentInfo, InstanceId, SessionEvent, SessionId};
use loom_client::{Client, Notification, memory_pair};
use loom_config::{AgentConfig, Dirs, PolicyAction, PolicyConfig, RelayServerConfig};
use loom_crypto::{HostIdentity, PairingUri, Pairings};
use loom_llm::{
    AssistantContent, AssistantMessage, ChunkStream, CompletionChunk, CompletionRequest,
    LlmProvider, ProviderId, StopReason, ToolCall, Usage,
};
use loom_mobile::{
    AgentConnection, AgentUpdate, ApprovalDecision, ConversationItem, ConversationUpdate, Delivery,
    Loom, LoomError, Policy, SessionStatus, TaskEdit, TaskState, ToolState,
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
}

impl Host {
    async fn start(replies: Vec<AssistantMessage>) -> Self {
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
        let (relay, relay_server) = serve_relay(listener, &identity);

        let mut config = AgentConfig::default();
        config.llm.default_model = Some(format!("{PROVIDER}/m"));
        config.policy = PolicyConfig {
            default: PolicyAction::Ask,
            rules: Vec::new(),
        };
        config.relay.enabled = true;
        config.relay.url = Some(format!("ws://{addr}"));
        let mut options = AgentOptions::new(&workspace, config.clone(), dirs);
        options.providers.push(Arc::new(Scripted {
            replies: Mutex::new(replies.into()),
        }));
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
        let agent_link = CancellationToken::new();
        let endpoint = relay_endpoint(&config).unwrap().unwrap();
        let mut handle = start_relay(ctx.clone(), endpoint, keys, &data, agent_link.clone());
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
        (self.relay, self.relay_server) = serve_relay(listener, &self.identity);
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
) -> (CancellationToken, Arc<loom_relay::Relay>) {
    let config = RelayServerConfig {
        allowed_hosts: vec![identity.host_id().to_string()],
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

    fn idle(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|u| u.status == SessionStatus::Idle)
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

    // The session's own default for tool calls reaches the conversation.
    assert_eq!(app.last.as_ref().unwrap().policy, None);
    conn.clone()
        .set_policy(session.id.clone(), Some(Policy::Allow))
        .await
        .unwrap();
    app.until(&conn, "the session's policy", |app| {
        app.last.as_ref().unwrap().policy == Some(Policy::Allow)
    })
    .await;
    conn.clone()
        .set_policy(session.id.clone(), None)
        .await
        .unwrap();
    app.until(&conn, "the agent's policy", |app| {
        app.last.as_ref().unwrap().policy.is_none()
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
