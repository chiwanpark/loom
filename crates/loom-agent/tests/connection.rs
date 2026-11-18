use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use loom_agent::server::{ServerContext, serve_stream};
use loom_agent_core::{Agent, AgentOptions};
use loom_client::proto::{
    AgentInfo, ClientHello, ClientMessage, Command, ContentPart, Decision, Delivery, ErrorCode,
    EventWindow, InstanceId, Notice, PROTOCOL_VERSION, PolicyAction, ReasoningEffort,
    ServerMessage, SessionEvent, SessionId, Task, TaskChange, TaskStatus, TerminalClient,
    TerminalHolder, TerminalInfo, TurnOutcome, decode, encode,
};
use loom_client::{Client, ClientError, Notification, Notifications, memory_pair};
use loom_config::{AgentConfig, Dirs};
use loom_llm::{
    AssistantContent, AssistantMessage, ChunkStream, CompletionChunk, CompletionRequest,
    LlmProvider, ProviderId, StopReason, ToolCall, Usage,
};
use serde_json::json;

const PROVIDER: &str = "scripted";

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
            arguments: json!({"path": path, "content": "hi"}),
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
    async fn usage_limits(&self) -> loom_llm::Result<Option<loom_llm::UsageLimits>> {
        Ok(Some(loom_llm::UsageLimits {
            limits: vec![loom_llm::UsageLimit {
                label: "5h".into(),
                used_percent: Some(42),
                detail: None,
                resets_at: Some(1_794_330_000),
                headline: true,
            }],
        }))
    }
}

struct Setup {
    agent: Agent,
    info: AgentInfo,
    dir: tempfile::TempDir,
}

fn setup(replies: Vec<AssistantMessage>) -> Setup {
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
    let mut config = AgentConfig::default();
    config.llm.default_model = Some(format!("{PROVIDER}/m"));
    config.terminal.shell = Some("/bin/sh".into());
    let mut options = AgentOptions::new(&workspace, config, dirs);
    options.providers.push(Arc::new(Scripted {
        replies: Mutex::new(replies.into()),
    }));
    let agent = Agent::new(options).unwrap();
    let info = AgentInfo {
        instance: InstanceId("test-instance".into()),
        workspace,
        pid: std::process::id(),
        started_at: 0,
        version: "test".into(),
        default_model: None,
    };
    Setup { agent, info, dir }
}

/// A connection's notifications with event batches split into one event
/// each, so tests can wait for events one by one.
struct Notes {
    inner: Notifications,
    split: VecDeque<Notification>,
}

impl Notes {
    async fn recv(&mut self) -> Option<Notification> {
        if let Some(note) = self.split.pop_front() {
            return Some(note);
        }
        match self.inner.recv().await? {
            Notification::Events {
                session,
                events,
                mut window,
            } if events.len() > 1 => {
                for event in events {
                    self.split.push_back(Notification::Events {
                        session: session.clone(),
                        events: vec![event],
                        window: window.take(),
                    });
                }
                self.split.pop_front()
            }
            note => Some(note),
        }
    }
}

async fn connect(setup: &Setup) -> (Client, Notes) {
    let (client_side, server_side) = memory_pair();
    tokio::spawn(serve_stream(
        ServerContext::new(setup.agent.clone(), setup.info.clone()),
        server_side,
    ));
    let (client, inner) = Client::connect(client_side, "test").await.unwrap();
    (
        client,
        Notes {
            inner,
            split: VecDeque::new(),
        },
    )
}

async fn next_event(notes: &mut Notes) -> (SessionId, u64, SessionEvent) {
    loop {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("connection closed");
        if let Notification::Events {
            session, events, ..
        } = note
            && let Some(event) = events.into_iter().next()
        {
            return (session, event.seq, event.event);
        }
    }
}

async fn until_turn_finished(notes: &mut Notes) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    loop {
        let (_, _, event) = next_event(notes).await;
        let done = matches!(event, SessionEvent::TurnFinished { .. });
        events.push(event);
        if done {
            return events;
        }
    }
}

#[tokio::test]
async fn prompt_turn_streams_events_to_subscribers() {
    let setup = setup(vec![text("Hello from the agent")]);
    let (client, mut notes) = connect(&setup).await;
    assert_eq!(client.agent().instance.0, "test-instance");
    assert_eq!(client.agent().default_model.as_deref(), Some("scripted/m"));

    let session = client.create_session(None).await.unwrap();
    assert_eq!(session.model, None);
    let info = client.subscribe(&session.id, None, None).await.unwrap();
    assert_eq!(info.id, session.id);
    client.send_text(&session.id, "hi").await.unwrap();
    let events = until_turn_finished(&mut notes).await;
    assert!(matches!(&events[0], SessionEvent::UserMessage { .. }));
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::AssistantMessage { text, .. } if text == "Hello from the agent")
    ));
    assert!(matches!(
        events.last(),
        Some(SessionEvent::TurnFinished {
            outcome: TurnOutcome::Completed
        })
    ));

    let sessions = client.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].last_seq > 0);

    let (other, mut other_notes) = connect(&setup).await;
    other.subscribe(&session.id, Some(1), None).await.unwrap();
    let (_, seq, _) = next_event(&mut other_notes).await;
    assert_eq!(seq, 2);

    let metrics = client.metrics().await.unwrap();
    assert_eq!(metrics.sessions, 1);
    assert_eq!(metrics.busy_sessions, 0);
    assert_eq!(metrics.turns_completed, 1);
    assert_eq!(metrics.connections, 2);
    drop(other);
    drop(other_notes);
    for _ in 0..100 {
        if client.metrics().await.unwrap().connections == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let metrics = client.metrics().await.unwrap();
    assert_eq!(metrics.connections, 1);
    assert_eq!(metrics.connections_total, 2);
    assert_eq!(
        client.telemetry_status().await.unwrap(),
        loom_client::proto::TelemetryStatus::Off
    );
}

#[tokio::test]
async fn approvals_round_trip_through_the_client() {
    let setup = setup(vec![write_call("out.txt"), text("written")]);
    let (client, mut notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap();
    client.subscribe(&session.id, None, None).await.unwrap();
    let info = client
        .set_policy(&session.id, Some(PolicyAction::Ask))
        .await
        .unwrap();
    assert_eq!(info.policy_default, PolicyAction::Ask);
    client.send_text(&session.id, "write it").await.unwrap();
    let call_id = loop {
        if let (_, _, SessionEvent::ApprovalRequired { call_id, .. }) = next_event(&mut notes).await
        {
            break call_id;
        }
    };
    let err = client
        .resolve_approval(&session.id, "missing", Decision::Allow)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    client
        .resolve_approval(&session.id, &call_id, Decision::Allow)
        .await
        .unwrap();
    let events = until_turn_finished(&mut notes).await;
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::ToolCallFinished { result, .. } if !result.is_error
    )));
    assert_eq!(
        std::fs::read_to_string(setup.info.workspace.join("out.txt")).unwrap(),
        "hi"
    );
}

/// Messages sent during a turn wait for it, can be taken back, and start
/// the next turn.
#[tokio::test]
async fn messages_wait_for_the_running_turn() {
    let setup = setup(vec![
        write_call("out.txt"),
        text("written"),
        text("later on"),
    ]);
    let (client, mut notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap().id;
    client.subscribe(&session, None, None).await.unwrap();
    client
        .set_policy(&session, Some(PolicyAction::Ask))
        .await
        .unwrap();
    client.send_text(&session, "write it").await.unwrap();
    let call_id = loop {
        if let (_, _, SessionEvent::ApprovalRequired { call_id, .. }) = next_event(&mut notes).await
        {
            break call_id;
        }
    };
    let message = |text: &str| vec![loom_client::proto::ContentPart::text(text)];
    let err = client
        .send_prompt(&session, message("now"))
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Busy);
    for text in ["later", "never mind"] {
        client
            .send_message(&session, message(text), Delivery::Queue)
            .await
            .unwrap();
    }
    let mut waiting = Vec::new();
    while waiting.len() < 2 {
        if let (_, _, SessionEvent::PendingAdded { message }) = next_event(&mut notes).await {
            waiting.push(message);
        }
    }
    let taken = client
        .cancel_pending(&session, waiting[1].id)
        .await
        .unwrap();
    assert_eq!(taken.content, message("never mind"));
    let err = client
        .cancel_pending(&session, waiting[1].id)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);

    client
        .resolve_approval(&session, &call_id, Decision::Deny)
        .await
        .unwrap();
    until_turn_finished(&mut notes).await;
    let next = until_turn_finished(&mut notes).await;
    let users: Vec<_> = next
        .iter()
        .filter_map(|e| match e {
            SessionEvent::UserMessage { content } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(users, [message("later")]);
}

#[tokio::test]
async fn sessions_are_cleared_through_the_client() {
    let setup = setup(vec![write_call("out.txt"), text("written")]);
    let (client, mut notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap();
    client.subscribe(&session.id, None, None).await.unwrap();
    client
        .set_policy(&session.id, Some(PolicyAction::Ask))
        .await
        .unwrap();
    client.send_text(&session.id, "write it").await.unwrap();
    let call_id = loop {
        if let (_, _, SessionEvent::ApprovalRequired { call_id, .. }) = next_event(&mut notes).await
        {
            break call_id;
        }
    };
    let err = client.clear_session(&session.id).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Busy);
    client
        .resolve_approval(&session.id, &call_id, Decision::Deny)
        .await
        .unwrap();
    until_turn_finished(&mut notes).await;

    client.clear_session(&session.id).await.unwrap();
    let seq = loop {
        if let (id, seq, SessionEvent::Cleared) = next_event(&mut notes).await {
            assert_eq!(id, session.id);
            break seq;
        }
    };
    let (other, mut other_notes) = connect(&setup).await;
    other.subscribe(&session.id, None, None).await.unwrap();
    let (_, first, event) = next_event(&mut other_notes).await;
    assert_eq!((first, event), (seq, SessionEvent::Cleared));

    let missing = SessionId("nope".into());
    let err = client.clear_session(&missing).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn sessions_are_rewound_through_the_client() {
    let setup = setup(vec![text("one done"), text("two done")]);
    let (client, mut notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap();
    client.subscribe(&session.id, None, None).await.unwrap();
    let mut users = Vec::new();
    for prompt in ["one", "two"] {
        client.send_text(&session.id, prompt).await.unwrap();
        let seq = loop {
            if let (_, seq, SessionEvent::UserMessage { .. }) = next_event(&mut notes).await {
                break seq;
            }
        };
        users.push(seq);
        until_turn_finished(&mut notes).await;
    }

    let content = client.rewind_session(&session.id, users[1]).await.unwrap();
    assert_eq!(content, vec![ContentPart::text("two")]);
    loop {
        if let (_, _, SessionEvent::Rewound { seq }) = next_event(&mut notes).await {
            assert_eq!(seq, users[1]);
            break;
        }
    }
    let err = client
        .rewind_session(&session.id, users[1])
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    let err = client
        .rewind_session(&SessionId("nope".into()), users[0])
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn worktree_sessions_through_the_client() {
    let setup = setup(vec![]);
    let ws = setup.info.workspace.clone();
    let git = |dir: &std::path::Path, args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&ws, &["init", "-q", "-b", "main"]);
    std::fs::write(ws.join("README"), "hello\n").unwrap();
    git(&ws, &["add", "-A"]);
    git(&ws, &["commit", "-q", "-m", "first"]);
    let (client, _notes) = connect(&setup).await;

    let session = client
        .create_worktree_session(None, loom_proto::WorktreeOptions::default())
        .await
        .unwrap();
    let worktree = session.worktree.clone().unwrap();
    let path = std::path::PathBuf::from(&worktree.path);
    std::fs::write(path.join("only-here.txt"), "x\n").unwrap();
    let names = |listing: loom_proto::DirectoryListing| -> Vec<String> {
        listing.entries.into_iter().map(|e| e.name).collect()
    };
    let scoped = client.in_session(Some(session.id.clone()));
    assert!(names(scoped.list_directory("").await.unwrap()).contains(&"only-here.txt".into()));
    assert!(!names(client.list_directory("").await.unwrap()).contains(&"only-here.txt".into()));
    let changes = scoped.workspace_changes().await.unwrap();
    assert_eq!(changes.branch.as_deref(), Some(worktree.branch.as_str()));
    assert!(changes.files.iter().any(|f| f.path == "only-here.txt"));
    let committed = changes.committed.unwrap();
    assert_eq!(committed.base, "main");
    assert!(committed.commits.is_empty() && committed.files.is_empty());

    let status = client.worktree_status(&session.id).await.unwrap();
    assert!(status.dirty);
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "work"]);
    let changes = scoped.workspace_changes().await.unwrap();
    assert!(changes.files.is_empty());
    let committed = changes.committed.unwrap();
    assert_eq!(committed.commits.len(), 1);
    assert_eq!(committed.files[0].path, "only-here.txt");
    let diff = scoped.file_diff("only-here.txt", None, true).await.unwrap();
    assert!(diff.diff.contains("+x\n"), "{}", diff.diff);
    assert!(
        client
            .workspace_changes()
            .await
            .unwrap()
            .committed
            .is_none()
    );

    let err = client.delete_session(&session.id).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Conflict);
    client.remove_worktree(&session.id, true).await.unwrap();
    assert!(!path.exists());
    let gone = client.delete_session(&session.id).await.unwrap_err();
    assert_eq!(gone.api().unwrap().code, ErrorCode::NotFound);
    assert!(
        !client
            .list_sessions()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id)
    );

    let missing = client
        .in_session(Some(SessionId("nope".into())))
        .list_directory("")
        .await
        .unwrap_err();
    assert_eq!(missing.api().unwrap().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn session_settings_and_errors() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let session = client.create_session(Some("scripted/other")).await.unwrap();
    assert_eq!(session.model.as_deref(), Some("scripted/other"));
    let info = client
        .set_reasoning(&session.id, Some(ReasoningEffort::High))
        .await
        .unwrap();
    assert_eq!(info.reasoning, Some(ReasoningEffort::High));
    let info = client.set_model(&session.id, None).await.unwrap();
    assert_eq!(info.model, None);

    let missing = SessionId("nope".into());
    let err = client.send_text(&missing, "hi").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    let err = client.send_text(&session.id, "  ").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let err = client
        .create_session(Some("unknown-provider/x"))
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let err = client.unsubscribe(&session.id).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    assert!(!client.interrupt(&session.id).await.unwrap());
    let err = client.compact_session(&session.id).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    assert!(err.to_string().contains("nothing to compact"), "{err}");
    let err = client.compact_session(&missing).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    let err = client.list_models("unknown-provider").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let err = client.cancel_login("nope").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    let err = client.list_agents().await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Unsupported);
}

#[tokio::test]
async fn usage_limits_reach_the_client() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let limits = client.usage_limits(PROVIDER).await.unwrap().unwrap();
    assert_eq!(
        limits.limits,
        vec![loom_proto::UsageLimit {
            label: "5h".into(),
            used_percent: Some(42),
            detail: None,
            resets_at: Some(1_794_330_000),
            headline: true,
        }]
    );
    let err = client.usage_limits("unknown-provider").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn provider_status_and_api_keys_are_generic() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let providers = client.list_providers().await.unwrap();
    assert!(!providers.is_empty());
    assert!(providers.iter().all(|p| p.enabled && !p.signed_in()));
    let with_key = providers.iter().find(|p| {
        p.login_methods
            .contains(&loom_client::proto::LoginMethodInfo::ApiKey)
    });
    if let Some(provider) = with_key {
        client
            .set_api_key(&provider.provider, Some("test-key".into()))
            .await
            .unwrap();
        let after = client.list_providers().await.unwrap();
        let status = after
            .iter()
            .find(|p| p.provider == provider.provider)
            .unwrap();
        assert!(status.api_key_set);
        assert!(setup.dir.path().join("data/loom/credentials.json").exists());
    }
    let err = client.logout("unknown-provider").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn notices_reach_connected_clients() {
    let setup = setup(vec![]);
    let (_client, mut notes) = connect(&setup).await;
    // The list of terminals comes first, after the hello.
    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        note,
        Notification::Notice(Notice::TerminalsChanged(Vec::new()))
    );
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(5), notes.recv())
            .await
            .unwrap()
            .unwrap()
    };
    let mut config = (*setup.agent.config()).clone();
    config.policy.default = loom_config::PolicyAction::Deny;
    setup.agent.apply_config(config).unwrap();
    assert_eq!(
        next().await,
        Notification::Notice(Notice::ConfigReloaded {
            restart_required: vec![]
        })
    );
    setup.agent.shutdown().await;
    assert_eq!(next().await, Notification::Notice(Notice::ShuttingDown));
}

async fn raw_exchange(setup: &Setup, messages: Vec<Bytes>) -> Vec<ServerMessage> {
    let (mut client_side, server_side) = memory_pair();
    let server = tokio::spawn(serve_stream(
        ServerContext::new(setup.agent.clone(), setup.info.clone()),
        server_side,
    ));
    for message in messages {
        client_side.send(message).await.unwrap();
    }
    let mut received = Vec::new();
    while let Ok(Ok(Some(frame))) =
        tokio::time::timeout(Duration::from_secs(5), client_side.recv()).await
    {
        received.push(decode(&frame).unwrap());
    }
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    received
}

fn frame(message: &ClientMessage) -> Bytes {
    Bytes::from(encode(message).unwrap())
}

#[tokio::test]
async fn handshake_violations_close_the_connection() {
    let setup = setup(vec![]);
    let request = ClientMessage::Request {
        id: 1,
        command: Command::ListSessions,
    };
    let replies = raw_exchange(&setup, vec![frame(&request)]).await;
    assert!(matches!(
        replies.as_slice(),
        [ServerMessage::Rejected(err)] if err.code == ErrorCode::Protocol
    ));

    let old = ClientMessage::Hello(ClientHello {
        version: PROTOCOL_VERSION + 1,
        client: "future".into(),
    });
    let replies = raw_exchange(&setup, vec![frame(&old)]).await;
    assert!(matches!(
        replies.as_slice(),
        [ServerMessage::Rejected(err)] if err.code == ErrorCode::VersionMismatch
    ));

    let hello = ClientMessage::Hello(ClientHello {
        version: PROTOCOL_VERSION,
        client: "raw".into(),
    });
    let replies = raw_exchange(&setup, vec![frame(&hello), Bytes::from_static(&[0xff; 4])]).await;
    assert!(matches!(replies.first(), Some(ServerMessage::Hello(_))));
    assert!(matches!(
        replies.last(),
        Some(ServerMessage::Rejected(err)) if err.code == ErrorCode::Protocol
    ));
}

#[tokio::test]
async fn dropped_connection_fails_pending_requests() {
    let setup = setup(vec![]);
    let (client_side, server_side) = memory_pair();
    let server = tokio::spawn(serve_stream(
        ServerContext::new(setup.agent.clone(), setup.info.clone()),
        server_side,
    ));
    let (client, _notes) = Client::connect(client_side, "test").await.unwrap();
    server.abort();
    let _ = server.await;
    let err = client.list_sessions().await.unwrap_err();
    assert!(matches!(err, ClientError::Closed), "{err:?}");
    assert!(client.is_closed());
}

async fn terminal_text_until(notes: &mut Notes, seen: &mut String, needle: &str) {
    while !seen.contains(needle) {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}; saw {seen:?}"))
            .expect("connection closed");
        if let Notification::TerminalOutput { data, .. } = note {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
}

#[tokio::test]
async fn interactive_terminals_run_in_the_workspace() {
    let setup = setup(vec![]);
    let (client, mut notes) = connect(&setup).await;
    let err = client.open_terminal(0, 24, 1).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let terminal = client.open_terminal(80, 24, 1).await.unwrap();
    client
        .terminal_input(terminal, b"echo answer=$((6*7)); pwd\n".to_vec())
        .await
        .unwrap();
    let mut text = String::new();
    terminal_text_until(&mut notes, &mut text, "answer=42").await;
    let workspace = setup.info.workspace.display().to_string();
    terminal_text_until(&mut notes, &mut text, &workspace).await;
    text.clear();
    client.resize_terminal(terminal, 100, 30).await.unwrap();
    client
        .terminal_input(terminal, b"stty size\n".to_vec())
        .await
        .unwrap();
    terminal_text_until(&mut notes, &mut text, "30 100").await;
    client
        .terminal_input(terminal, b"exit 7\n".to_vec())
        .await
        .unwrap();
    let status = loop {
        match tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Notification::TerminalExited {
                terminal: id,
                status,
            } if id == terminal => break status,
            _ => {}
        }
    };
    assert_eq!(status, 7);
    let err = client.close_terminal(terminal).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);

    let second = client.open_terminal(80, 24, 1).await.unwrap();
    assert_ne!(second, terminal);
    client.close_terminal(second).await.unwrap();
    let err = client
        .terminal_input(second, b"x".to_vec())
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn terminals_survive_a_reconnect() {
    let setup = setup(vec![]);
    let (client, mut notes) = connect(&setup).await;
    let terminal = client.open_terminal(80, 24, 1).await.unwrap();
    client
        .terminal_input(
            terminal,
            b"KEPT=yes; trap 'echo redraw=$((1+1))' WINCH; echo ready=$((2*2))\n".to_vec(),
        )
        .await
        .unwrap();
    let mut text = String::new();
    terminal_text_until(&mut notes, &mut text, "ready=4").await;
    drop(client);
    drop(notes);

    let (client, mut notes) = connect(&setup).await;
    // A new connection cannot use the terminal until it attaches.
    let err = client
        .terminal_input(terminal, b"x".to_vec())
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    client
        .attach_terminal(terminal, 80, 24, 1, false)
        .await
        .unwrap();
    // What it printed before comes again.
    terminal_text_until(&mut notes, &mut String::new(), "ready=4").await;
    let mut text = String::new();
    client
        .terminal_input(terminal, b"echo kept=$KEPT; stty size\n".to_vec())
        .await
        .unwrap();
    terminal_text_until(&mut notes, &mut text, "kept=yes").await;
    terminal_text_until(&mut notes, &mut text, "24 80").await;
    // The size did not change, so only the agent's explicit SIGWINCH runs the
    // trap; sh runs it once its pending read returns.
    terminal_text_until(&mut notes, &mut text, "redraw=2").await;
    client.close_terminal(terminal).await.unwrap();
    let err = client
        .attach_terminal(terminal, 80, 24, 1, false)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
}

/// Waits for the terminal list that `check` accepts.
async fn terminals_until(
    notes: &mut Notes,
    check: impl Fn(&[TerminalInfo]) -> bool,
) -> Vec<TerminalInfo> {
    loop {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .expect("timed out waiting for terminals")
            .expect("connection closed");
        if let Notification::Notice(Notice::TerminalsChanged(terminals)) = note
            && check(&terminals)
        {
            return terminals;
        }
    }
}

#[tokio::test]
async fn terminals_move_between_clients() {
    let setup = setup(vec![]);
    let (desktop, mut desktop_notes) = connect(&setup).await;
    let (phone, mut phone_notes) = connect(&setup).await;
    // Each connection hears of the terminals at once, none yet.
    terminals_until(&mut desktop_notes, |t| t.is_empty()).await;
    terminals_until(&mut phone_notes, |t| t.is_empty()).await;

    let terminal = desktop.open_terminal(80, 24, 1).await.unwrap();
    let holder = |list: &[TerminalInfo]| list.first().map(|t| (t.id, t.holder.clone()));
    let mine = terminals_until(&mut desktop_notes, |t| !t.is_empty()).await;
    assert_eq!(holder(&mine), Some((terminal, TerminalHolder::You)));
    let theirs = terminals_until(&mut phone_notes, |t| !t.is_empty()).await;
    let desktop_client = TerminalClient {
        program: "test".into(),
        label: None,
    };
    assert_eq!(
        holder(&theirs),
        Some((terminal, TerminalHolder::Other(desktop_client.clone())))
    );
    assert_eq!(phone.terminals().await.unwrap(), theirs);

    desktop
        .terminal_input(terminal, b"echo from=desk$((1))\n".to_vec())
        .await
        .unwrap();
    terminal_text_until(&mut desktop_notes, &mut String::new(), "from=desk1").await;

    // Another owner needs to take it over.
    let err = phone
        .attach_terminal(terminal, 60, 20, 2, false)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Conflict);
    assert!(err.to_string().contains("open on test"), "{err}");
    phone
        .attach_terminal(terminal, 60, 20, 2, true)
        .await
        .unwrap();
    // The phone sees what the desktop saw, then its own input.
    let mut seen = String::new();
    terminal_text_until(&mut phone_notes, &mut seen, "from=desk1").await;
    phone
        .terminal_input(terminal, b"stty size\n".to_vec())
        .await
        .unwrap();
    terminal_text_until(&mut phone_notes, &mut seen, "20 60").await;
    let list = phone.terminals().await.unwrap();
    assert_eq!(holder(&list), Some((terminal, TerminalHolder::You)));
    // The desktop hears that it moved.
    terminals_until(&mut desktop_notes, |t| {
        holder(t) == Some((terminal, TerminalHolder::Other(desktop_client.clone())))
    })
    .await;
    // The desktop lost it.
    let err = desktop
        .terminal_input(terminal, b"x".to_vec())
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    assert!(desktop.close_terminal(terminal).await.is_err());

    // A new connection of the phone's owner takes it back from the old one,
    // which the agent may not know is gone yet.
    let (phone_again, mut again_notes) = connect(&setup).await;
    phone_again
        .attach_terminal(terminal, 60, 20, 2, false)
        .await
        .unwrap();
    terminal_text_until(&mut again_notes, &mut String::new(), "20 60").await;
    drop(phone);
    drop(phone_notes);

    // When it closes, it leaves every list.
    phone_again.close_terminal(terminal).await.unwrap();
    terminals_until(&mut desktop_notes, |t| t.is_empty()).await;
    terminals_until(&mut again_notes, |t| t.is_empty()).await;
}

#[tokio::test]
async fn workspace_changes_come_from_git() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let plain = client.workspace_changes().await.unwrap();
    assert!(!plain.repository);

    let workspace = &setup.info.workspace;
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(workspace)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    std::fs::write(workspace.join("tracked.txt"), "a\n").unwrap();
    git(&["init", "-q"]);
    git(&["add", "tracked.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    std::fs::write(workspace.join("tracked.txt"), "b\n").unwrap();

    let changes = client.workspace_changes().await.unwrap();
    assert!(changes.repository);
    assert_eq!(changes.files.len(), 1);
    assert_eq!(changes.files[0].path, "tracked.txt");
    assert_eq!((changes.files[0].added, changes.files[0].removed), (1, 1));
    assert_eq!(changes.committed, None);
    let diff = client.file_diff("tracked.txt", None, false).await.unwrap();
    assert!(diff.diff.contains("-a\n+b\n"), "{}", diff.diff);
    let err = client
        .file_diff("/etc/passwd", None, false)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let err = client
        .file_diff("tracked.txt", None, true)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);

    git(&["branch", "upstream"]);
    git(&["branch", "--set-upstream-to=upstream"]);
    std::fs::write(workspace.join("added.txt"), "new\n").unwrap();
    git(&["add", "added.txt"]);
    git(&["commit", "-q", "-m", "Add a file"]);
    let changes = client.workspace_changes().await.unwrap();
    assert_eq!(changes.files.len(), 1);
    let committed = changes.committed.unwrap();
    assert_eq!(committed.base, "upstream");
    assert_eq!(committed.commits.len(), 1);
    assert_eq!(committed.commits[0].summary, "Add a file");
    assert_eq!(committed.commits[0].author, "T");
    assert_eq!(committed.files.len(), 1);
    assert_eq!(committed.files[0].path, "added.txt");
    assert_eq!(
        committed.files[0].status,
        loom_client::proto::ChangeStatus::Added
    );
    let diff = client.file_diff("added.txt", None, true).await.unwrap();
    assert!(diff.diff.contains("+new\n"), "{}", diff.diff);
}

#[tokio::test]
async fn unknown_requests_get_an_error_and_keep_the_connection() {
    let setup = setup(vec![]);
    let (mut client_side, server_side) = memory_pair();
    let server = tokio::spawn(serve_stream(
        ServerContext::new(setup.agent.clone(), setup.info.clone()),
        server_side,
    ));
    let hello = ClientMessage::Hello(ClientHello {
        version: PROTOCOL_VERSION,
        client: "raw".into(),
    });
    client_side.send(frame(&hello)).await.unwrap();
    let unknown = encode(&(1u32, 7u64, 9999u32)).unwrap();
    client_side.send(Bytes::from(unknown)).await.unwrap();
    let known = ClientMessage::Request {
        id: 8,
        command: Command::ListSessions,
    };
    client_side.send(frame(&known)).await.unwrap();
    let mut replies = Vec::new();
    for _ in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(5), client_side.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        replies.push(decode::<ServerMessage>(&frame).unwrap());
    }
    assert!(matches!(replies[0], ServerMessage::Hello(_)));
    assert!(matches!(
        &replies[1],
        ServerMessage::Response { id: 7, result: Err(err) } if err.code == ErrorCode::Unsupported
    ));
    assert!(matches!(
        &replies[2],
        ServerMessage::Response {
            id: 8,
            result: Ok(loom_proto::Payload::Sessions(_))
        }
    ));
    server.abort();
}

#[tokio::test]
async fn sessions_can_be_renamed_and_files_browsed() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap();
    let renamed = client
        .rename_session(&session.id, Some("My session".into()))
        .await
        .unwrap();
    assert_eq!(renamed.title.as_deref(), Some("My session"));
    let listed = client.list_sessions().await.unwrap();
    assert_eq!(listed[0].title.as_deref(), Some("My session"));

    let workspace = &setup.info.workspace;
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let top = client.list_directory("").await.unwrap();
    assert_eq!(top.entries.len(), 1);
    assert_eq!(top.entries[0].name, "src");
    let src = client.list_directory("src").await.unwrap();
    assert_eq!(src.entries[0].name, "lib.rs");
    let file = client.read_file("src/lib.rs").await.unwrap();
    assert_eq!(file.text, "pub fn f() {}\n");
    let err = client.read_file("../outside").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);

    let version = client
        .write_file("src/lib.rs", "pub fn g() {}\n".into(), file.version)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/lib.rs")).unwrap(),
        "pub fn g() {}\n"
    );
    assert_eq!(
        client.read_file("src/lib.rs").await.unwrap().version,
        version
    );
    // The version read before the save is out of date now.
    let err = client
        .write_file("src/lib.rs", "pub fn h() {}\n".into(), file.version)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Conflict);
}

/// The version of the next `NotesChanged` notice, skipping other
/// notifications.
async fn notes_changed(notes: &mut Notes) -> loom_client::proto::FileVersion {
    loop {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .expect("timed out waiting for a notes notice")
            .expect("connection closed");
        if let Notification::Notice(Notice::NotesChanged { version }) = note {
            return version;
        }
    }
}

#[tokio::test]
async fn notes_are_read_and_written_through_the_client() {
    let edit = reply(
        vec![AssistantContent::ToolCall(ToolCall {
            call_id: "call-1".into(),
            name: "notes_edit".into(),
            arguments: json!({"patch": "@@\n - plan\n+- step done\n"}),
            provider_data: None,
        })],
        StopReason::ToolUse,
    );
    let setup = setup(vec![edit, text("ok")]);
    let (client, mut notes) = connect(&setup).await;
    let (other, mut other_notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap().id;

    let empty = client.notes().await.unwrap();
    assert_eq!(empty.text, "");
    assert_eq!(empty.max_bytes, 64 * 1024);
    let version = client
        .write_notes("- plan".into(), empty.version)
        .await
        .unwrap();
    for notes in [&mut notes, &mut other_notes] {
        assert_eq!(notes_changed(notes).await, version);
    }
    let read = other.notes().await.unwrap();
    assert_eq!((read.text.as_str(), read.version), ("- plan", version));

    // A client that read the notes before the write is out of date.
    let err = other
        .write_notes("lost".into(), empty.version)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Conflict);

    // A session's agent changes them during a turn, without asking.
    client.subscribe(&session, None, None).await.unwrap();
    client
        .send_prompt(&session, vec![loom_client::proto::ContentPart::text("go")])
        .await
        .unwrap();
    let events = until_turn_finished(&mut notes).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    let version = notes_changed(&mut other_notes).await;
    let read = other.notes().await.unwrap();
    assert_eq!(read.text, "- plan\n- step done");
    assert_eq!(read.version, version);

    let err = client
        .write_notes("x".repeat(64 * 1024 + 1), version)
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);

    // The notes are the agent's: deleting the session keeps them.
    client.delete_session(&session).await.unwrap();
    assert_eq!(client.notes().await.unwrap().text, "- plan\n- step done");
    let cleared = client.write_notes(String::new(), version).await.unwrap();
    assert_eq!(cleared, empty.version);
    assert_eq!(client.notes().await.unwrap().text, "");
}

/// The next `TasksChanged` notice, skipping other notifications.
async fn tasks_changed(notes: &mut Notes) -> (SessionId, Vec<Task>) {
    loop {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .expect("timed out waiting for a tasks notice")
            .expect("connection closed");
        if let Notification::Notice(Notice::TasksChanged { session, tasks }) = note {
            return (session, tasks);
        }
    }
}

#[tokio::test]
async fn system_prompt_is_read_through_the_client() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    std::fs::write(setup.info.workspace.join("AGENTS.md"), "Use tabs.").unwrap();
    let session = client.create_session(None).await.unwrap().id;

    let prompt = client.system_prompt(&session).await.unwrap();
    assert!(prompt.text.starts_with("You are Loom"), "{}", prompt.text);
    assert!(prompt.text.contains("Use tabs."), "{}", prompt.text);
    assert!(prompt.warnings.is_empty(), "{:?}", prompt.warnings);
    let read = prompt.tools.iter().find(|t| t.name == "read").unwrap();
    let parameters: serde_json::Value = serde_json::from_str(&read.parameters).unwrap();
    assert_eq!(parameters["type"], "object");

    let err = client
        .system_prompt(&SessionId("missing".into()))
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
}

#[tokio::test]
async fn tasks_are_read_and_changed_through_the_client() {
    let update = reply(
        vec![AssistantContent::ToolCall(ToolCall {
            call_id: "call-1".into(),
            name: "tasks_update".into(),
            arguments: json!({"changes": [{"action": "update", "id": 1, "status": "done"}]}),
            provider_data: None,
        })],
        StopReason::ToolUse,
    );
    let setup = setup(vec![update, text("ok")]);
    let (client, mut notes) = connect(&setup).await;
    let (other, mut other_notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap().id;

    let empty = client.tasks(&session).await.unwrap();
    assert!(empty.tasks.is_empty());
    assert_eq!(empty.max_tasks, 100);
    let list = client
        .update_tasks(
            &session,
            vec![TaskChange::Add {
                text: "plan".into(),
                status: TaskStatus::InProgress,
            }],
        )
        .await
        .unwrap();
    assert_eq!(list.tasks.len(), 1);
    for notes in [&mut notes, &mut other_notes] {
        assert_eq!(
            tasks_changed(notes).await,
            (session.clone(), list.tasks.clone())
        );
    }
    assert_eq!(other.tasks(&session).await.unwrap(), list);

    let err = other
        .update_tasks(&session, vec![TaskChange::Remove { id: 5 }])
        .await
        .unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);

    // The agent changes it during a turn, without asking.
    client.subscribe(&session, None, None).await.unwrap();
    client
        .send_prompt(&session, vec![loom_client::proto::ContentPart::text("go")])
        .await
        .unwrap();
    let events = until_turn_finished(&mut notes).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    let (changed, tasks) = tasks_changed(&mut other_notes).await;
    assert_eq!(changed, session);
    assert_eq!(tasks[0].status, TaskStatus::Done);
    assert_eq!(other.tasks(&session).await.unwrap().tasks, tasks);

    client.delete_session(&session).await.unwrap();
    let err = client.tasks(&session).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
}

/// Names in a directory, to see that no partial download stays behind.
fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn umask() -> u32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("Umask:")).unwrap();
    u32::from_str_radix(line["Umask:".len()..].trim(), 8).unwrap()
}

#[tokio::test]
async fn files_download_in_chunks_to_a_local_path() {
    use std::os::unix::fs::PermissionsExt;

    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let workspace = &setup.info.workspace;
    // Several chunks and a partial one, with bytes that are not text.
    let len = 3 * 1024 * 1024 + 12345;
    let bytes: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
    std::fs::write(workspace.join("data.bin"), &bytes).unwrap();
    std::fs::write(workspace.join("empty"), b"").unwrap();
    let out = setup.dir.path().join("downloads");
    std::fs::create_dir(&out).unwrap();
    // An existing file is replaced.
    std::fs::write(out.join("data.bin"), "old").unwrap();

    let mut seen = Vec::new();
    let size = client
        .download_file("data.bin", &out.join("data.bin"), |done, total| {
            seen.push((done, total))
        })
        .await
        .unwrap();
    assert_eq!(size, len as u64);
    assert_eq!(std::fs::read(out.join("data.bin")).unwrap(), bytes);
    let mode = std::fs::metadata(out.join("data.bin"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o666 & !umask());
    let mib = 1024 * 1024;
    assert_eq!(
        seen,
        vec![
            (mib, len as u64),
            (2 * mib, len as u64),
            (3 * mib, len as u64),
            (len as u64, len as u64)
        ]
    );
    assert_eq!(
        client
            .read_file_bytes("data.bin", len as u64)
            .await
            .unwrap(),
        bytes
    );
    assert!(client.read_file_bytes("empty", 0).await.unwrap().is_empty());

    let size = client
        .download_file("empty", &out.join("empty"), |_, _| {})
        .await
        .unwrap();
    assert_eq!(size, 0);
    assert_eq!(std::fs::read(out.join("empty")).unwrap(), b"");

    for (path, code) in [
        ("missing", ErrorCode::NotFound),
        ("../outside", ErrorCode::BadRequest),
        ("", ErrorCode::BadRequest),
    ] {
        let err = client
            .download_file(path, &out.join("failed"), |_, _| {})
            .await
            .unwrap_err();
        assert_eq!(err.api().map(|e| e.code), Some(code), "{path}: {err}");
    }
    assert_eq!(entries(&out), vec!["data.bin", "empty"]);
}

#[tokio::test]
async fn a_file_that_changes_during_a_download_is_not_saved() {
    use std::io::{Seek, SeekFrom, Write};

    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let source = setup.info.workspace.join("log.txt");
    let out = setup.dir.path().join("downloads");
    std::fs::create_dir(&out).unwrap();

    // The file is rewritten in place, keeping its size, or grows, after the
    // first chunk arrived and before the next ones are asked for.
    for grow in [false, true] {
        std::fs::write(&source, vec![b'a'; 2 * 1024 * 1024 + 1]).unwrap();
        // An old time, so that the change shows even with coarse timestamps.
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000))
            .unwrap();
        let err = client
            .download_file("log.txt", &out.join("log.txt"), |_, _| {
                let mut file = std::fs::File::options().write(true).open(&source).unwrap();
                if grow {
                    file.seek(SeekFrom::End(0)).unwrap();
                    file.write_all(b"more").unwrap();
                } else {
                    file.seek(SeekFrom::Start(1024 * 1024)).unwrap();
                    file.write_all(b"b").unwrap();
                }
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ClientError::Changed(path) if path == "log.txt"),
            "grow {grow}: {err}"
        );
        assert!(entries(&out).is_empty());
    }
}

#[tokio::test]
async fn a_dropped_download_leaves_nothing_behind() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    std::fs::write(
        setup.info.workspace.join("big.bin"),
        vec![1u8; 8 * 1024 * 1024],
    )
    .unwrap();
    let out = setup.dir.path().join("downloads");
    std::fs::create_dir(&out).unwrap();

    let (started, mut cancel) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let dest = out.join("big.bin");
    let download = client.download_file("big.bin", &dest, |_, _| {
        if let Some(started) = started.take() {
            let _ = started.send(());
        }
    });
    tokio::select! {
        biased;
        _ = &mut cancel => {}
        _ = download => panic!("the download finished before it was cancelled"),
    }
    assert!(entries(&out).is_empty());
    // The connection still works.
    assert!(client.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn files_upload_in_chunks_to_the_workspace() {
    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    let workspace = &setup.info.workspace;
    std::fs::create_dir(workspace.join("assets")).unwrap();
    let local = setup.dir.path().join("local");
    std::fs::create_dir(&local).unwrap();
    let len = 3 * 1024 * 1024 + 12345;
    let bytes: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
    std::fs::write(local.join("data.bin"), &bytes).unwrap();
    std::fs::write(local.join("empty"), b"").unwrap();

    let mut seen = Vec::new();
    let size = client
        .upload_file(
            &local.join("data.bin"),
            "assets/data.bin",
            false,
            |done, total| seen.push((done, total)),
        )
        .await
        .unwrap();
    assert_eq!(size, len as u64);
    assert_eq!(
        std::fs::read(workspace.join("assets/data.bin")).unwrap(),
        bytes
    );
    let mib = 1024 * 1024;
    assert_eq!(
        seen,
        vec![
            (mib, len as u64),
            (2 * mib, len as u64),
            (3 * mib, len as u64),
            (len as u64, len as u64)
        ]
    );
    let size = client
        .upload_file(&local.join("empty"), "empty", false, |_, _| {})
        .await
        .unwrap();
    assert_eq!(size, 0);
    assert_eq!(std::fs::read(workspace.join("empty")).unwrap(), b"");

    // An existing file is replaced only when asked.
    let err = client
        .upload_file(&local.join("empty"), "assets/data.bin", false, |_, _| {})
        .await
        .unwrap_err();
    assert_eq!(
        err.api().map(|e| e.code),
        Some(ErrorCode::Conflict),
        "{err}"
    );
    assert_eq!(
        std::fs::read(workspace.join("assets/data.bin")).unwrap(),
        bytes
    );
    client
        .upload_file(&local.join("empty"), "assets/data.bin", true, |_, _| {})
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(workspace.join("assets/data.bin")).unwrap(),
        b""
    );

    for (path, code) in [
        ("missing/a.bin", ErrorCode::NotFound),
        ("../outside", ErrorCode::BadRequest),
        ("", ErrorCode::BadRequest),
    ] {
        let err = client
            .upload_file(&local.join("data.bin"), path, false, |_, _| {})
            .await
            .unwrap_err();
        assert_eq!(err.api().map(|e| e.code), Some(code), "{path}: {err}");
    }
    assert_eq!(entries(&workspace.join("assets")), vec!["data.bin"]);

    // A dropped upload leaves nothing behind.
    let (started, mut cancel) = tokio::sync::oneshot::channel();
    let mut started = Some(started);
    let src = local.join("data.bin");
    let upload = client.upload_file(&src, "assets/big.bin", false, |_, _| {
        if let Some(started) = started.take() {
            let _ = started.send(());
        }
    });
    tokio::select! {
        biased;
        _ = &mut cancel => {}
        _ = upload => panic!("the upload finished before it was cancelled"),
    }
    // The cancel goes out before this request, so it is done once this is.
    assert!(client.list_sessions().await.unwrap().is_empty());
    for _ in 0..100 {
        if entries(&workspace.join("assets")) == vec!["data.bin"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(entries(&workspace.join("assets")), vec!["data.bin"]);
}

#[tokio::test]
async fn files_are_read_in_chunks_through_a_relay() {
    let setup = setup(vec![]);
    std::fs::write(setup.info.workspace.join("a.txt"), "a").unwrap();
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (server_read, server_write) = tokio::io::split(server_io);
    let peer = loom_transport::PeerInfo::Relay {
        label: "laptop".into(),
        pubkey: [0; 32],
    };
    tokio::spawn(serve_stream(
        ServerContext::new(setup.agent.clone(), setup.info.clone()),
        loom_transport::FrameStream::new(server_read, server_write, peer),
    ));
    let stream = loom_transport::FrameStream::new(
        client_read,
        client_write,
        loom_transport::PeerInfo::Memory,
    );
    let (client, _notes) = Client::connect(stream, "test").await.unwrap();

    let out = setup.dir.path().join("a.txt");
    let size = client
        .download_file("a.txt", &out, |_, _| {})
        .await
        .unwrap();
    assert_eq!(size, 1);
    assert_eq!(std::fs::read(&out).unwrap(), b"a");
    assert_eq!(client.read_file_bytes("a.txt", 1).await.unwrap(), b"a");
    let err = client.read_file_bytes("a.txt", 0).await.unwrap_err();
    assert!(
        matches!(
            err,
            loom_client::ClientError::TooLarge {
                size: 1,
                max: 0,
                ..
            }
        ),
        "{err}"
    );
}

/// Waits for the next batch of events, as the agent sent it.
async fn next_batch(
    notes: &mut Notifications,
) -> (Vec<loom_client::proto::SequencedEvent>, Option<EventWindow>) {
    loop {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .expect("timed out waiting for events")
            .expect("connection closed");
        if let Notification::Events { events, window, .. } = note {
            return (events, window);
        }
    }
}

#[tokio::test]
async fn a_tail_subscription_sends_the_latest_items_in_one_batch_and_pages_back() {
    let setup = setup(Vec::new());
    let (client, mut notes) = connect(&setup).await;
    let session = client.create_session(None).await.unwrap();
    client.subscribe(&session.id, None, None).await.unwrap();
    for turn in 0..30 {
        client
            .send_text(&session.id, &format!("turn {turn}"))
            .await
            .unwrap();
        until_turn_finished(&mut notes).await;
    }

    // This test looks at the batches as they come.
    let (other, Notes { inner: mut raw, .. }) = connect(&setup).await;
    // Everything, for comparison.
    other.subscribe(&session.id, None, None).await.unwrap();
    let (all, window) = next_batch(&mut raw).await;
    assert!(!window.unwrap().has_older);
    other.unsubscribe(&session.id).await.unwrap();

    // Each turn is a prompt and a reply: ten items are the last five turns,
    // in one message with the window.
    other.subscribe(&session.id, None, Some(10)).await.unwrap();
    let (mut seen, window) = next_batch(&mut raw).await;
    let window = window.expect("a new window");
    assert!(window.has_older);
    assert!(
        seen.len() < all.len() / 4,
        "{} of {}",
        seen.len(),
        all.len()
    );
    assert_eq!(seen.last(), all.last());
    assert!(matches!(
        &seen[0].event,
        SessionEvent::UserMessage { content } if content[0] == ContentPart::text("turn 25")
    ));

    let mut has_older = true;
    while has_older {
        let page = other.history(&session.id, seen[0].seq, 10).await.unwrap();
        has_older = page.has_older;
        seen.splice(0..0, page.events);
    }
    assert_eq!(seen, all);

    // A short gap is caught up without a new window.
    other.unsubscribe(&session.id).await.unwrap();
    let since = all[all.len() - 3].seq;
    other
        .subscribe(&session.id, Some(since), Some(10))
        .await
        .unwrap();
    let (caught_up, window) = next_batch(&mut raw).await;
    assert!(window.is_none());
    assert_eq!(caught_up, all[all.len() - 2..]);
}

/// A server that answers each connection with what it sent, reversed, once
/// the client ended its side, and then `extra` more bytes.
async fn reversing_server(extra: usize) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut received = Vec::new();
                stream.read_to_end(&mut received).await.unwrap();
                received.reverse();
                stream.write_all(&received).await.unwrap();
                stream.write_all(&vec![7u8; extra]).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    });
    port
}

#[tokio::test]
async fn local_ports_are_forwarded_to_the_agents_host() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let setup = setup(vec![]);
    let (client, _notes) = connect(&setup).await;
    // More than the window each way, so both wait for room.
    let port = reversing_server(3 * 1024 * 1024).await;

    let ports = client.list_ports().await.unwrap();
    let listed = ports
        .iter()
        .find(|p| p.port == port)
        .expect("the port is listed");
    assert_eq!(listed.pid, Some(std::process::id()));
    assert_eq!(listed.connect_host(), "127.0.0.1");

    let forward_client = client.clone();
    let forward = loom_client::PortForward::start(None, "127.0.0.1".into(), port, move || {
        Some(forward_client.clone())
    })
    .await
    .unwrap();
    // The agent's port is taken here, as both run on this machine.
    assert_ne!(forward.local_addr().port(), port);

    let sent: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let mut connections = Vec::new();
    for _ in 0..3 {
        let mut stream = tokio::net::TcpStream::connect(forward.local_addr())
            .await
            .unwrap();
        let sent = sent.clone();
        connections.push(tokio::spawn(async move {
            let (mut read, mut write) = stream.split();
            let reading = async {
                let mut received = Vec::new();
                read.read_to_end(&mut received).await.unwrap();
                received
            };
            let writing = async {
                write.write_all(&sent).await.unwrap();
                write.shutdown().await.unwrap();
            };
            tokio::join!(reading, writing).0
        }));
    }
    let mut expected = sent.clone();
    expected.reverse();
    expected.extend(vec![7u8; 3 * 1024 * 1024]);
    for connection in connections {
        let received = tokio::time::timeout(Duration::from_secs(30), connection)
            .await
            .expect("the forwarded connection finished")
            .unwrap();
        assert!(received == expected, "{} bytes", received.len());
    }

    // A port nobody listens on closes the local connection at once.
    drop(forward);
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let nobody = closed.local_addr().unwrap().port();
    drop(closed);
    let err = client.open_tunnel("127.0.0.1", nobody).await.err().unwrap();
    assert_eq!(
        err.api().map(|e| e.code),
        Some(ErrorCode::NotFound),
        "{err}"
    );
    // The connection still works.
    assert!(client.list_sessions().await.unwrap().is_empty());
}
