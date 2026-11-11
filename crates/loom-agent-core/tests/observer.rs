mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, ScriptedProvider, calls, collect_until, run_turn, test_config, text};
use loom_agent_core::{Agent, AgentNotice, AgentOptions, SessionObserver};
use loom_config::PolicyAction;
use loom_proto::{SessionEvent, SessionId, SessionState};
use serde_json::json;

#[derive(Default)]
struct Recorder {
    created: Mutex<Vec<SessionId>>,
    events: Mutex<Vec<(SessionId, SessionEvent)>>,
}

impl SessionObserver for Recorder {
    fn session_created(&self, session: &SessionId) {
        self.created.lock().unwrap().push(session.clone());
    }

    fn event(&self, session: &SessionId, event: &SessionEvent) {
        self.events
            .lock()
            .unwrap()
            .push((session.clone(), event.clone()));
    }
}

fn agent_with(h: &Harness, recorder: &Arc<Recorder>, scripts: Vec<common::Script>) -> Agent {
    let mut options =
        AgentOptions::new(&h.workspace, test_config(PolicyAction::Ask), h.dirs.clone());
    options.providers.push(ScriptedProvider::new(scripts));
    options.observer = Some(recorder.clone());
    Agent::new(options).unwrap()
}

// Observers run without the session lock; if a turn held it while changing
// state, `info()` would deadlock and this test would hang.
#[tokio::test]
async fn state_changes_are_announced_to_every_client() {
    let h = Harness::new(test_config(PolicyAction::Ask), vec![text("hi")]);
    let mut notices = h.agent.notices();
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hello").await;

    let mut states = Vec::new();
    while !states.ends_with(&[SessionState::Idle]) || states.len() < 2 {
        let notice = tokio::time::timeout(Duration::from_secs(10), notices.recv())
            .await
            .expect("timed out waiting for a notice")
            .unwrap();
        if let AgentNotice::SessionUpdated(info) = notice {
            assert_eq!(info.id, *session.id());
            states.push(info.state);
        }
    }
    // The first notice announces the new session; the rest follow its state.
    assert_eq!(
        states,
        [
            SessionState::Idle,
            SessionState::Thinking,
            SessionState::Idle
        ]
    );
}

#[tokio::test]
async fn observers_see_live_events_but_not_restored_ones() {
    let h = Harness::new(test_config(PolicyAction::Ask), Vec::new());
    let recorder = Arc::new(Recorder::default());
    let agent = agent_with(
        &h,
        &recorder,
        vec![calls(&[(
            "c1",
            "write",
            json!({"path": "a.txt", "content": "x"}),
        )])],
    );
    let session = agent.create_session(None).unwrap();
    assert_eq!(*recorder.created.lock().unwrap(), [session.id().clone()]);
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![loom_proto::ContentPart::text("go")])
        .unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    agent.flush().await;

    let seen: Vec<_> = recorder
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|(id, event)| {
            assert_eq!(id, session.id());
            event.clone()
        })
        .filter(|event| !event.is_ephemeral())
        .collect();
    let recorded: Vec<_> = session
        .subscribe(None)
        .backlog
        .into_iter()
        .map(|e| e.event)
        .collect();
    assert_eq!(seen, recorded);

    // Reopening repairs the unfinished turn, which must not be reported again.
    let id = session.id().clone();
    drop(agent);
    let reopened = Arc::new(Recorder::default());
    let agent = agent_with(&h, &reopened, vec![text("ok")]);
    assert!(reopened.events.lock().unwrap().is_empty());
    assert!(reopened.created.lock().unwrap().is_empty());

    let restored = agent.session(&id).unwrap();
    let mut sub = restored.subscribe(Some(restored.info().last_seq));
    run_turn(&restored, &mut sub, "again").await;
    let events = reopened.events.lock().unwrap();
    assert!(matches!(
        events.first(),
        Some((_, SessionEvent::UserMessage { .. }))
    ));
    assert!(reopened.created.lock().unwrap().is_empty());
}
