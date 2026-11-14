//! Messages sent while a turn runs: queued ones start the next turn, and
//! steering ones go into the running turn at its next step.

mod common;

use std::sync::Arc;

use common::{Harness, Script, calls, collect_until, gated, test_config, text};
use loom_agent_core::{CoreError, Sent, Session, Subscription};
use loom_config::PolicyAction;
use loom_llm::{InputContent, Message};
use loom_proto::{
    ContentPart, Decision, Delivery, SessionEvent, SessionState, TurnOutcome, apply_pending,
};
use serde_json::json;

fn send(session: &Session, text: &str, delivery: Delivery) -> Sent {
    session
        .send_message(vec![ContentPart::text(text)], delivery)
        .unwrap()
}

async fn until_finished(sub: &mut Subscription) -> Vec<SessionEvent> {
    collect_until(sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await
}

async fn until_thinking(sub: &mut Subscription) {
    collect_until(sub, |e| {
        matches!(
            e,
            SessionEvent::StateChanged {
                state: SessionState::Thinking
            }
        )
    })
    .await;
}

/// The texts of the user messages among `events`, in order.
fn user_texts(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::UserMessage { content } => content.iter().find_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                ContentPart::Image { .. } => None,
            }),
            _ => None,
        })
        .collect()
}

fn outcome(events: &[SessionEvent]) -> Option<TurnOutcome> {
    events.iter().find_map(|e| match e {
        SessionEvent::TurnFinished { outcome } => Some(*outcome),
        _ => None,
    })
}

/// The text of a request's last message, which must be the user's.
fn last_user_text(messages: &[Message]) -> String {
    match messages.last() {
        Some(Message::User { content }) => match &content[0] {
            InputContent::Text { text } => text.clone(),
            other => panic!("not text: {other:?}"),
        },
        other => panic!("the last message is not the user's: {other:?}"),
    }
}

#[tokio::test]
async fn steering_follows_the_tool_results_and_the_queue_the_turn() {
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![
            calls(&[("c1", "shell", json!({"command": "echo hi"}))]),
            text("steered"),
            text("queued answer"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![ContentPart::text("start")])
        .unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    assert_eq!(
        send(&session, "steer me", Delivery::Steer),
        Sent::Pending(1)
    );
    assert_eq!(send(&session, "later", Delivery::Queue), Sent::Pending(2));
    assert!(matches!(
        session.send_message(vec![ContentPart::text("now")], Delivery::Now),
        Err(CoreError::Busy)
    ));
    assert_eq!(session.pending().len(), 2);
    session.resolve_approval("c1", Decision::Allow).unwrap();

    let first = until_finished(&mut sub).await;
    assert_eq!(outcome(&first), Some(TurnOutcome::Completed));
    // The prompt's event came before the approval.
    assert_eq!(user_texts(&first), ["steer me"]);
    let finished = first
        .iter()
        .position(|e| matches!(e, SessionEvent::ToolCallFinished { .. }))
        .unwrap();
    let delivered = first
        .iter()
        .position(|e| matches!(e, SessionEvent::PendingRemoved { id: 1 }))
        .unwrap();
    assert!(finished < delivered);
    assert!(matches!(
        first[delivered + 1],
        SessionEvent::UserMessage { .. }
    ));

    let second = until_finished(&mut sub).await;
    assert!(matches!(second[0], SessionEvent::PendingRemoved { id: 2 }));
    assert_eq!(user_texts(&second), ["later"]);
    assert_eq!(outcome(&second), Some(TurnOutcome::Completed));
    session.wait_idle().await;
    assert!(session.pending().is_empty());

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    // The steering message follows the tool result in the same turn.
    let steered = &requests[1].messages;
    assert!(matches!(
        steered[steered.len() - 2],
        Message::ToolResult { .. }
    ));
    assert_eq!(last_user_text(steered), "steer me");
    assert_eq!(last_user_text(&requests[2].messages), "later");
}

/// A steering message that comes while the model answers without tools
/// makes the turn go on rather than end.
#[tokio::test]
async fn steering_during_an_answer_continues_the_turn() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![gated(&gate, text("first answer")), text("after steering")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session.send_prompt(vec![ContentPart::text("go")]).unwrap();
    until_thinking(&mut sub).await;
    send(&session, "also this", Delivery::Steer);
    gate.notify_one();

    let events = until_finished(&mut sub).await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert_eq!(user_texts(&events), ["also this"]);
    let answers: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["first answer", "after steering"]);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(last_user_text(&requests[1].messages), "also this");
}

/// After an interrupted turn the waiting messages stay; they go on after
/// the next turn that completes.
#[tokio::test]
async fn waiting_messages_are_held_after_an_interrupt() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![Script::Hang, text("now"), text("held")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session.send_prompt(vec![ContentPart::text("a")]).unwrap();
    until_thinking(&mut sub).await;
    send(&session, "held", Delivery::Queue);
    session.interrupt();
    let interrupted = until_finished(&mut sub).await;
    assert_eq!(outcome(&interrupted), Some(TurnOutcome::Interrupted));
    session.wait_idle().await;
    assert_eq!(session.info().state, SessionState::Idle);
    assert_eq!(session.pending().len(), 1);

    assert_eq!(send(&session, "now", Delivery::Queue), Sent::Started);
    let now = until_finished(&mut sub).await;
    assert_eq!(user_texts(&now), ["now"]);
    let held = until_finished(&mut sub).await;
    assert_eq!(user_texts(&held), ["held"]);
    assert_eq!(outcome(&held), Some(TurnOutcome::Completed));
    assert!(session.pending().is_empty());
}

/// Waiting messages can be taken back, outlast a restart, and stay through
/// clearing the conversation; a new window tells of them.
#[tokio::test]
async fn waiting_messages_are_cancelled_kept_and_told_of() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(config.clone(), vec![Script::Hang]);
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session.send_prompt(vec![ContentPart::text("a")]).unwrap();
    until_thinking(&mut sub).await;
    send(&session, "x", Delivery::Queue);
    send(&session, "y", Delivery::Steer);
    let taken = session.cancel_pending(1).unwrap();
    assert_eq!(taken.content, vec![ContentPart::text("x")]);
    assert!(matches!(
        session.cancel_pending(1),
        Err(CoreError::UnknownPending(1))
    ));
    let id = session.id().clone();
    h.agent.shutdown().await;
    drop(session);

    let (agent, _) = h.reopen(config, vec![]);
    let session = agent.session(&id).unwrap();
    let pending = session.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, 2);
    assert_eq!(pending[0].delivery, Delivery::Steer);
    let tail = session.subscribe_tail(None, Some(1));
    let mut shown = tail.window.unwrap().pending;
    for event in &tail.backlog {
        apply_pending(&mut shown, &event.event);
    }
    assert_eq!(shown, pending);

    session.clear().unwrap();
    let after = session.subscribe(None);
    let kinds: Vec<&SessionEvent> = after.backlog.iter().map(|e| &e.event).collect();
    assert!(matches!(kinds[0], SessionEvent::Cleared));
    assert!(
        matches!(kinds[1], SessionEvent::PendingAdded { message } if message.id == 2),
        "{kinds:?}"
    );
    assert_eq!(session.pending(), pending);
}
