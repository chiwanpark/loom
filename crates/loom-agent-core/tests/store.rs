mod common;

use common::{Harness, MODEL_REF, calls, collect_until, run_turn, test_config, text};
use loom_config::PolicyAction;
use loom_llm::Message;
use loom_proto::{SessionEvent, SessionState, TurnOutcome};
use serde_json::json;

fn kinds(events: &[loom_proto::SequencedEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e.event {
            SessionEvent::UserMessage { .. } => "user",
            SessionEvent::AssistantDelta { .. } => "delta",
            SessionEvent::ReasoningDelta { .. } => "reasoning",
            SessionEvent::AssistantMessage { .. } => "assistant",
            SessionEvent::ToolCallRequested { .. } => "requested",
            SessionEvent::ApprovalRequired { .. } => "approval",
            SessionEvent::ApprovalResolved { .. } => "resolved",
            SessionEvent::ToolCallStarted { .. } => "started",
            SessionEvent::ToolOutput { .. } => "output",
            SessionEvent::ToolCallFinished { .. } => "finished",
            SessionEvent::StateChanged { .. } => "state",
            SessionEvent::Usage { .. } => "usage",
            SessionEvent::TurnFinished { .. } => "turn",
            SessionEvent::Warning { .. } => "warning",
            SessionEvent::Error { .. } => "error",
            SessionEvent::ContextCompacted { .. } => "compacted",
        })
        .collect()
}

#[tokio::test]
async fn sessions_survive_a_restart() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(
        config.clone(),
        vec![
            calls(&[("c1", "write", json!({"path": "a.txt", "content": "x"}))]),
            text("written"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    session.set_reasoning(Some(loom_llm::ReasoningEffort::High));
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "Write a file\nplease").await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    let before = session.subscribe(None).backlog;
    let history = session.history();
    h.agent.shutdown().await;
    drop(session);

    let (agent, provider) = h.reopen(config, vec![text("again")]);
    let sessions = agent.list_sessions();
    assert_eq!(sessions.len(), 1);
    let info = &sessions[0];
    assert_eq!(info.title.as_deref(), Some("Write a file"));
    assert_eq!(info.model.as_deref(), Some(MODEL_REF));
    assert_eq!(info.reasoning, Some(loom_proto::ReasoningEffort::High));
    assert_eq!(info.state, SessionState::Idle);
    assert!(info.created_at > 0);

    let restored = agent.session(&info.id).unwrap();
    assert_eq!(restored.history(), history);
    let backlog = restored.subscribe(None).backlog;
    let persisted: Vec<_> = before
        .iter()
        .filter(|e| !e.event.is_ephemeral())
        .cloned()
        .collect();
    assert_eq!(backlog, persisted);
    assert!(!kinds(&backlog).contains(&"delta"));
    assert!(kinds(&backlog).contains(&"finished"));

    let mut sub = restored.subscribe(None);
    sub.backlog.clear();
    let events = run_turn(&restored, &mut sub, "and again").await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    let request = provider.requests().pop().unwrap();
    assert_eq!(request.messages.len(), history.len() + 1);
    let next = restored.subscribe(None).backlog;
    assert!(next.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[tokio::test]
async fn interrupted_turns_are_repaired_on_restore() {
    let config = test_config(PolicyAction::Ask);
    let h = Harness::new(
        config.clone(),
        vec![calls(&[
            ("c1", "write", json!({"path": "a.txt", "content": "x"})),
            ("c2", "read", json!({"path": "a.txt"})),
        ])],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![loom_proto::ContentPart::text("go")])
        .unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    h.agent.flush().await;
    let id = session.id().clone();

    let (agent, provider) = h.reopen(config.clone(), vec![text("ok")]);
    let restored = agent.session(&id).unwrap();
    let history = restored.history();
    let results: Vec<_> = history
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult {
                call_id, is_error, ..
            } => Some((call_id.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![("c1".to_owned(), true), ("c2".to_owned(), true)]
    );
    let backlog = restored.subscribe(None).backlog;
    let tail = kinds(&backlog);
    assert_eq!(&tail[tail.len() - 3..], ["warning", "turn", "state"]);
    assert!(matches!(
        backlog[backlog.len() - 2].event,
        SessionEvent::TurnFinished {
            outcome: TurnOutcome::Interrupted
        }
    ));
    assert_eq!(restored.info().state, SessionState::Idle);

    let mut sub = restored.subscribe(None);
    sub.backlog.clear();
    let events = run_turn(&restored, &mut sub, "continue").await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    assert_eq!(provider.requests().len(), 1);
    agent.shutdown().await;

    let (again, _) = h.reopen(config, vec![]);
    let backlog = again.session(&id).unwrap().subscribe(None).backlog;
    assert_eq!(
        kinds(&backlog).iter().filter(|k| **k == "warning").count(),
        1
    );
    drop(h);
}

#[tokio::test]
async fn persistence_can_be_disabled() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![]);
    h.agent.create_session(None).unwrap();
    h.agent.shutdown().await;
    let provider = common::ScriptedProvider::new(vec![]);
    let mut options = loom_agent_core::AgentOptions::new(&h.workspace, config, h.dirs.clone());
    options.providers.push(provider);
    options.persist = false;
    let agent = loom_agent_core::Agent::new(options).unwrap();
    assert!(agent.list_sessions().is_empty());
}

#[tokio::test]
async fn archived_sessions_stay_archived_and_are_announced() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![]);
    let mut notices = h.agent.notices();
    let kept = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let archived = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let info = h.agent.set_archived(archived.id(), true).unwrap();
    assert!(info.archived);
    let mut announced = Vec::new();
    while let Ok(notice) = notices.try_recv() {
        if let loom_agent_core::AgentNotice::SessionUpdated(info) = notice {
            announced.push((info.id.clone(), info.archived));
        }
    }
    assert_eq!(
        announced,
        vec![
            (kept.id().clone(), false),
            (archived.id().clone(), false),
            (archived.id().clone(), true),
        ]
    );
    h.agent.shutdown().await;

    let (agent, _) = h.reopen(config, vec![]);
    let sessions = agent.list_sessions();
    assert_eq!(sessions.len(), 2);
    let flags: Vec<_> = sessions
        .iter()
        .map(|s| (s.id.clone(), s.archived))
        .collect();
    assert!(flags.contains(&(kept.id().clone(), false)));
    assert!(flags.contains(&(archived.id().clone(), true)));
    let restored = agent.set_archived(archived.id(), false).unwrap();
    assert!(!restored.archived);
}

#[tokio::test]
async fn deleted_sessions_are_removed_from_the_store() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![text("hello")]);
    let kept = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let doomed = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = doomed.subscribe(None);
    let events = run_turn(&doomed, &mut sub, "hi").await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    let mut notices = h.agent.notices();
    h.agent.delete_session(doomed.id()).unwrap();
    assert!(h.agent.session(doomed.id()).is_err());
    assert!(h.agent.delete_session(doomed.id()).is_err());
    assert!(doomed.removed().is_cancelled());
    assert_eq!(
        notices.try_recv().unwrap(),
        loom_agent_core::AgentNotice::SessionRemoved(doomed.id().clone())
    );
    doomed.set_reasoning(Some(loom_llm::ReasoningEffort::High));
    let ids: Vec<_> = h.agent.list_sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![kept.id().clone()]);
    h.agent.shutdown().await;

    let (agent, _) = h.reopen(config, vec![]);
    let ids: Vec<_> = agent.list_sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![kept.id().clone()]);
}
