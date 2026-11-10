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
    let err = client.list_models("unknown-provider").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::BadRequest);
    let err = client.cancel_login("nope").await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::NotFound);
    let err = client.list_agents().await.unwrap_err();
    assert_eq!(err.api().unwrap().code, ErrorCode::Unsupported);
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
