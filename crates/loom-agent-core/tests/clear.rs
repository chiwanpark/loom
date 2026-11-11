mod common;

use common::{
    Harness, MODEL_REF, Script, calls, collect_until, next_event, outcome, run_turn, test_config,
    text,
};
use loom_agent_core::CoreError;
use loom_config::PolicyAction;
use loom_llm::Message;
use loom_proto::{ContentPart, Decision, SessionEvent, SessionState, TurnOutcome};
use serde_json::json;

fn write(id: &str, content: &str) -> Script {
    calls(&[(id, "write", json!({"path": "a.txt", "content": content}))])
}

#[tokio::test]
async fn clearing_starts_the_conversation_over_and_survives_a_restart() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(
        config.clone(),
        vec![
            write("c1", "first"),
            text("written"),
            write("c2", "second"),
            text("written again"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "Write a file\nplease").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    collect_until(&mut sub, |e| {
        matches!(
            e,
            SessionEvent::StateChanged {
                state: SessionState::Idle
            }
        )
    })
    .await;
    // Would be announced as a change before the next response if the session
    // still measured against the one before the clear.
    session.set_reasoning(Some(loom_llm::ReasoningEffort::High));
    // The file changed since the session wrote it: a write would be refused
    // as stale if the session still remembered the old one.
    std::fs::write(h.workspace.join("a.txt"), "changed by someone else").unwrap();

    session.clear().unwrap();
    assert_eq!(next_event(&mut sub).await, SessionEvent::Cleared);
    assert!(session.history().is_empty());
    let backlog = session.subscribe(None).backlog;
    assert_eq!(backlog.len(), 1);
    assert_eq!(backlog[0].event, SessionEvent::Cleared);
    let cleared_seq = backlog[0].seq;
    assert!(cleared_seq > 1);
    let info = session.info();
    assert_eq!(info.title.as_deref(), Some("Write a file"));
    assert_eq!(info.model.as_deref(), Some(MODEL_REF));
    assert_eq!(info.reasoning, Some(loom_proto::ReasoningEffort::High));

    let requests_before = h.provider.requests().len();
    let events = run_turn(&session, &mut sub, "Again").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCallFinished { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error, "{}", results[0].content);
    assert_eq!(
        std::fs::read_to_string(h.workspace.join("a.txt")).unwrap(),
        "second"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ModelChanged { .. })),
        "{events:?}"
    );
    let first = &h.provider.requests()[requests_before];
    assert_eq!(first.messages.len(), 1, "{:?}", first.messages);
    assert!(matches!(&first.messages[0], Message::User { .. }));
    let history = session.history();
    let before = session.subscribe(None).backlog;
    h.agent.shutdown().await;
    drop(session);

    let (agent, _) = h.reopen(config, vec![]);
    let sessions = agent.list_sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title.as_deref(), Some("Write a file"));
    let restored = agent.session(&sessions[0].id).unwrap();
    assert_eq!(restored.history(), history);
    let backlog = restored.subscribe(None).backlog;
    let persisted: Vec<_> = before
        .into_iter()
        .filter(|e| !e.event.is_ephemeral())
        .collect();
    assert_eq!(backlog, persisted);
    assert_eq!(backlog[0].seq, cleared_seq);
    assert_eq!(backlog[0].event, SessionEvent::Cleared);
    assert!(backlog.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[tokio::test]
async fn renamed_sessions_keep_their_name_when_cleared() {
    let h = Harness::new(test_config(PolicyAction::Allow), vec![text("hi")]);
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "First prompt").await;
    h.agent
        .rename_session(session.id(), Some("My session".into()))
        .unwrap();
    session.clear().unwrap();
    assert_eq!(session.info().title.as_deref(), Some("My session"));
    // An empty session clears too, and has no title to keep.
    let empty = h.agent.create_session(None).unwrap();
    empty.clear().unwrap();
    assert_eq!(empty.info().title, None);
}

#[tokio::test]
async fn a_busy_session_is_not_cleared() {
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![write("c1", "x"), text("ok")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![ContentPart::text("write it")])
        .unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    assert!(matches!(session.clear(), Err(CoreError::Busy)));
    assert!(!session.history().is_empty());
    session.resolve_approval("c1", Decision::Deny).unwrap();
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert!(!events.contains(&SessionEvent::Cleared));
    session.clear().unwrap();
    assert!(session.history().is_empty());
}
