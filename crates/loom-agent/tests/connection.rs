use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use loom_agent::server::{ServerContext, serve_stream};
use loom_agent_core::{Agent, AgentOptions};
use loom_client::proto::{
    AgentInfo, ClientHello, ClientMessage, Command, Decision, ErrorCode, InstanceId, Notice,
    PROTOCOL_VERSION, PolicyAction, ReasoningEffort, ServerMessage, SessionEvent, SessionId,
    TurnOutcome, decode, encode,
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

async fn connect(setup: &Setup) -> (Client, Notifications) {
    let (client_side, server_side) = memory_pair();
    tokio::spawn(serve_stream(
        ServerContext::new(setup.agent.clone(), setup.info.clone()),
        server_side,
    ));
    Client::connect(client_side, "test").await.unwrap()
}

async fn next_event(notes: &mut Notifications) -> (SessionId, u64, SessionEvent) {
    loop {
        let note = tokio::time::timeout(Duration::from_secs(10), notes.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("connection closed");
        if let Notification::Event { session, event } = note {
            return (session, event.seq, event.event);
        }
    }
}

async fn until_turn_finished(notes: &mut Notifications) -> Vec<SessionEvent> {
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
    let info = client.subscribe(&session.id, None).await.unwrap();
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
    other.subscribe(&session.id, Some(1)).await.unwrap();
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
    client.subscribe(&session.id, None).await.unwrap();
    let info = client
        .set_policy(&session.id, Some(PolicyAction::Ask))
        .await
        .unwrap();
    assert_eq!(info.policy_default, Some(PolicyAction::Ask));
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
    let mut config = (*setup.agent.config()).clone();
    config.policy.default = loom_config::PolicyAction::Deny;
    setup.agent.apply_config(config).unwrap();
    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        note,
        Notification::Notice(Notice::ConfigReloaded {
            restart_required: vec![]
        })
    );
    setup.agent.shutdown().await;
    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note, Notification::Notice(Notice::ShuttingDown));
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

async fn terminal_text_until(notes: &mut Notifications, seen: &mut String, needle: &str) {
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
    let err = client.open_terminal(0, 24).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let terminal = client.open_terminal(80, 24).await.unwrap();
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

    let second = client.open_terminal(80, 24).await.unwrap();
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
    let terminal = client.open_terminal(80, 24).await.unwrap();
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
    client.attach_terminal(terminal, 80, 24).await.unwrap();
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
    let err = client.attach_terminal(terminal, 80, 24).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
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
    let diff = client.file_diff("tracked.txt", None).await.unwrap();
    assert!(diff.diff.contains("-a\n+b\n"), "{}", diff.diff);
    let err = client.file_diff("/etc/passwd", None).await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
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
