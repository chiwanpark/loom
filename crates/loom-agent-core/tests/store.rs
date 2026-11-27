mod common;

use common::{Harness, MODEL_REF, calls, collect_until, run_turn, test_config, text};
use loom_config::PolicyAction;
use loom_llm::Message;
use loom_proto::{ReasoningEffort, SessionEvent, SessionState, TurnOutcome};
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
            SessionEvent::ModelChanged { .. } => "model",
            SessionEvent::Cleared => "cleared",
            SessionEvent::PendingAdded { .. } => "pending",
            SessionEvent::PendingRemoved { .. } => "delivered",
            SessionEvent::Rewound { .. } => "rewound",
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
    session.set_reasoning(loom_llm::ReasoningEffort::High);
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
    assert_eq!(info.reasoning, loom_proto::ReasoningEffort::High);
    // Nobody saw the completed turn before the restart.
    assert_eq!(info.state, SessionState::Completed);
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
    let h = Harness::new(config.clone(), vec![text("hello")]);
    let kept = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let archived = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = archived.subscribe(None);
    run_turn(&archived, &mut sub, "hi").await;
    let mut notices = h.agent.notices();
    let info = h.agent.set_archived(archived.id(), true).unwrap();
    assert!(info.archived);
    let mut announced = Vec::new();
    while let Ok(notice) = notices.try_recv() {
        if let loom_agent_core::AgentNotice::SessionUpdated(info) = notice {
            announced.push((info.id.clone(), info.archived));
        }
    }
    assert_eq!(announced, vec![(archived.id().clone(), true)]);
    let listed: Vec<_> = h
        .agent
        .archived_sessions()
        .into_iter()
        .map(|s| (s.id, s.title, s.location))
        .collect();
    assert_eq!(
        listed,
        vec![(
            archived.id().clone(),
            Some("hi".to_owned()),
            loom_proto::ArchivedLocation::Agent
        )]
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
    assert!(agent.archived_sessions().is_empty());
}

#[tokio::test]
async fn an_empty_session_is_deleted_when_it_is_archived() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![]);
    let empty = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut notices = h.agent.notices();
    let info = h.agent.set_archived(empty.id(), true).unwrap();
    assert!(info.archived);
    assert_eq!(
        notices.try_recv().unwrap(),
        loom_agent_core::AgentNotice::SessionRemoved(empty.id().clone())
    );
    assert!(h.agent.session(empty.id()).is_err());
    assert!(h.agent.archived_sessions().is_empty());
    h.agent.shutdown().await;

    let (agent, _) = h.reopen(config, vec![]);
    assert!(agent.list_sessions().is_empty());
}

#[tokio::test]
async fn an_imported_session_keeps_its_conversation_and_tasks() {
    let config = test_config(PolicyAction::Allow);
    let source = Harness::new(config.clone(), vec![text("hello")]);
    let session = source.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "Fix the parser").await;
    session
        .update_tasks(vec![loom_proto::TaskChange::Add {
            text: "write tests".into(),
            status: loom_proto::TaskStatus::InProgress,
        }])
        .unwrap();
    source.agent.set_archived(session.id(), true).unwrap();
    source.agent.shutdown().await;
    let path = loom_agent_core::store::store_path(
        &loom_agent_core::data_dir(&config, &source.dirs),
        &source.workspace.canonicalize().unwrap(),
    );
    let (_, stored) = loom_agent_core::store::EventStore::open(&path, &source.workspace).unwrap();
    let stored = stored.sessions.into_iter().next().unwrap();
    assert!(stored.state.archived);

    let target = Harness::new(config.clone(), vec![text("again")]);
    let info = target.agent.import_session(stored.clone()).unwrap();
    assert_eq!(info.id, *session.id());
    assert!(!info.archived);
    assert_eq!(info.title.as_deref(), Some("Fix the parser"));
    let imported = target.agent.session(&info.id).unwrap();
    assert_eq!(imported.history(), session.history());
    assert_eq!(imported.tasks(), session.tasks());
    let events = imported.subscribe(None).backlog;
    assert_eq!(kinds(&events), kinds(&stored.events));
    let again = target.agent.import_session(stored).unwrap();
    assert_ne!(again.id, info.id);
    let mut sub = imported.subscribe(None);
    sub.backlog.clear();
    let events = run_turn(&imported, &mut sub, "Go on").await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    target.agent.shutdown().await;

    let (agent, _) = target.reopen(config, vec![]);
    let reopened = agent.session(&info.id).unwrap();
    assert!(!reopened.is_archived());
    assert_eq!(reopened.tasks().len(), 1);
    assert_eq!(reopened.history().len(), 4);
}

#[tokio::test]
async fn renamed_sessions_keep_their_title() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![text("hello")]);
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "Original prompt").await;
    let info = h
        .agent
        .rename_session(session.id(), Some("  Renamed session  ".into()))
        .unwrap();
    assert_eq!(info.title.as_deref(), Some("Renamed session"));
    h.agent.shutdown().await;
    drop(session);

    let (agent, _) = h.reopen(config, vec![]);
    let sessions = agent.list_sessions();
    assert_eq!(sessions[0].title.as_deref(), Some("Renamed session"));
    let reset = agent
        .rename_session(&sessions[0].id, Some("   ".into()))
        .unwrap();
    assert_eq!(reset.title.as_deref(), Some("Original prompt"));
    let reset = agent.rename_session(&sessions[0].id, None).unwrap();
    assert_eq!(reset.title.as_deref(), Some("Original prompt"));
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
    doomed.set_reasoning(loom_llm::ReasoningEffort::High);
    let ids: Vec<_> = h.agent.list_sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![kept.id().clone()]);
    h.agent.shutdown().await;

    let (agent, _) = h.reopen(config, vec![]);
    let ids: Vec<_> = agent.list_sessions().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![kept.id().clone()]);
}

fn model_changes(events: &[SessionEvent]) -> Vec<(String, ReasoningEffort)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ModelChanged { model, reasoning } => Some((model.clone(), *reasoning)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_restart_keeps_what_a_model_change_is_measured_against() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![text("one")]);
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "one").await;
    // Changed after the last response: only the saved state knows the
    // effort that response used.
    session.set_reasoning(loom_llm::ReasoningEffort::High);
    let id = session.id().clone();
    h.agent.shutdown().await;
    drop(session);

    let (agent, _) = h.reopen(config, vec![text("two"), text("three")]);
    let restored = agent.session(&id).unwrap();
    let mut sub = restored.subscribe(None);
    sub.backlog.clear();
    let events = run_turn(&restored, &mut sub, "two").await;
    assert_eq!(
        model_changes(&events),
        [(MODEL_REF.to_owned(), ReasoningEffort::High)]
    );
    let events = run_turn(&restored, &mut sub, "three").await;
    assert_eq!(model_changes(&events), []);
    agent.shutdown().await;
}

/// A session saved before the agent kept the last model measures a change
/// against the model of its last response.
#[tokio::test]
async fn an_older_session_measures_a_change_against_its_last_response() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![text("one")]);
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "one").await;
    session.set_model("scripted/other").unwrap();
    let id = session.id().clone();
    h.agent.shutdown().await;
    drop(session);

    let sessions = loom_agent_core::data_dir(&config, &h.dirs).join("sessions");
    let path = std::fs::read_dir(sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
        .unwrap();
    let db = rusqlite::Connection::open(path).unwrap();
    let state: String = db
        .query_row("SELECT state FROM sessions WHERE id = ?1", [&id.0], |row| {
            row.get(0)
        })
        .unwrap();
    let mut state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert!(state.as_object_mut().unwrap().remove("last_used").is_some());
    db.execute(
        "UPDATE sessions SET state = ?1 WHERE id = ?2",
        [state.to_string(), id.0.clone()],
    )
    .unwrap();
    drop(db);

    let (agent, _) = h.reopen(config, vec![text("two")]);
    let restored = agent.session(&id).unwrap();
    let mut sub = restored.subscribe(None);
    sub.backlog.clear();
    let events = run_turn(&restored, &mut sub, "two").await;
    assert_eq!(
        model_changes(&events),
        [("scripted/other".to_owned(), ReasoningEffort::Medium)]
    );
    agent.shutdown().await;
}
