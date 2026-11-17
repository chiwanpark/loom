//! Rewinding a conversation to before one of its user messages.

mod common;

use common::{
    Harness, MODEL_REF, Script, calls, collect_until, next_event, outcome, run_turn, test_config,
    text,
};
use loom_agent_core::{CoreError, Session, Subscription};
use loom_config::PolicyAction;
use loom_llm::{InputContent, Message};
use loom_proto::{
    ContentPart, Delivery, PendingMessage, SequencedEvent, SessionEvent, SessionState, TurnOutcome,
    apply_pending,
};
use serde_json::json;

/// The sequence number of the user message with `text`.
fn user_seq(session: &Session, text: &str) -> u64 {
    session
        .subscribe(None)
        .backlog
        .iter()
        .find(|e| {
            matches!(&e.event, SessionEvent::UserMessage { content }
                if content == &vec![ContentPart::text(text)])
        })
        .unwrap_or_else(|| panic!("no user message {text:?}"))
        .seq
}

fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => content.iter().find_map(|part| match part {
                InputContent::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

fn logged_users(events: &[SequencedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            SessionEvent::UserMessage { content } => content.iter().find_map(|part| match part {
                ContentPart::Text { text } => Some(text.clone()),
                ContentPart::Image { .. } => None,
            }),
            _ => None,
        })
        .collect()
}

fn logged_pending(events: &[SequencedEvent]) -> Vec<PendingMessage> {
    let mut pending = Vec::new();
    for event in events {
        apply_pending(&mut pending, &event.event);
    }
    pending
}

async fn until_completed(sub: &mut Subscription) {
    collect_until(sub, |e| {
        matches!(
            e,
            SessionEvent::StateChanged {
                state: SessionState::Completed
            }
        )
    })
    .await;
}

#[tokio::test]
async fn rewinding_forgets_the_message_and_what_followed_but_not_the_files() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(
        config.clone(),
        vec![
            text("one done"),
            calls(&[(
                "c1",
                "write",
                json!({"path": "a.txt", "content": "from two"}),
            )]),
            text("two done"),
            text("three done"),
            text("again done"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    for prompt in ["one", "two", "three"] {
        let events = run_turn(&session, &mut sub, prompt).await;
        assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    }
    until_completed(&mut sub).await;
    let two = user_seq(&session, "two");

    let content = session.rewind(two).unwrap();
    assert_eq!(content, vec![ContentPart::text("two")]);
    assert_eq!(
        next_event(&mut sub).await,
        SessionEvent::Rewound { seq: two }
    );
    assert_eq!(user_texts(&session.history()), ["one"]);
    assert_eq!(session.history().len(), 2);
    let backlog = session.subscribe(None).backlog;
    assert!(backlog[..backlog.len() - 1].iter().all(|e| e.seq < two));
    assert_eq!(
        backlog.last().unwrap().event,
        SessionEvent::Rewound { seq: two }
    );
    assert_eq!(logged_users(&backlog), ["one"]);
    assert_eq!(session.info().title.as_deref(), Some("one"));
    // The files stay as the forgotten turns left them.
    assert_eq!(
        std::fs::read_to_string(h.workspace.join("a.txt")).unwrap(),
        "from two"
    );

    let requests_before = h.provider.requests().len();
    let events = run_turn(&session, &mut sub, "again").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    let request = &h.provider.requests()[requests_before];
    assert_eq!(user_texts(&request.messages), ["one", "again"]);

    let history = session.history();
    let before: Vec<_> = session
        .subscribe(None)
        .backlog
        .into_iter()
        .filter(|e| !e.event.is_ephemeral())
        .collect();
    h.agent.shutdown().await;
    drop(session);
    let (agent, _) = h.reopen(config, vec![]);
    let restored = agent.session(&agent.list_sessions()[0].id).unwrap();
    assert_eq!(restored.history(), history);
    assert_eq!(restored.subscribe(None).backlog, before);
}

#[tokio::test]
async fn only_user_messages_of_an_idle_session_are_rewound() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![text("one done"), Script::Hang],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "one").await;
    let one = user_seq(&session, "one");
    let answer = session
        .subscribe(None)
        .backlog
        .iter()
        .find(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }))
        .unwrap()
        .seq;
    assert!(matches!(
        session.rewind(answer),
        Err(CoreError::UnknownMessage(seq)) if seq == answer
    ));
    assert!(matches!(
        session.rewind(9999),
        Err(CoreError::UnknownMessage(9999))
    ));

    session.send_prompt(vec![ContentPart::text("two")]).unwrap();
    collect_until(&mut sub, |e| {
        matches!(
            e,
            SessionEvent::StateChanged {
                state: SessionState::Thinking
            }
        )
    })
    .await;
    assert!(matches!(session.rewind(one), Err(CoreError::Busy)));
    session.interrupt();
    session.wait_idle().await;
    assert_eq!(session.rewind(one).unwrap(), vec![ContentPart::text("one")]);
    assert!(session.history().is_empty());
}

#[tokio::test]
async fn messages_that_a_summary_replaced_are_not_rewound() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.window_tokens = Some(1000);
    config.context.compaction_threshold = 0.5;
    let first = match text("first answer") {
        Script::Reply(mut message) => {
            message.usage.input = 900;
            Script::Reply(message)
        }
        other => other,
    };
    let h = Harness::new(
        config,
        vec![
            first,
            text("SUMMARY: the user said hello"),
            text("second answer"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hello").await;
    run_turn(&session, &mut sub, "continue").await;
    assert_eq!(session.history().len(), 3);

    let hello = user_seq(&session, "hello");
    assert!(matches!(
        session.rewind(hello),
        Err(CoreError::MessageCompacted)
    ));
    let cont = user_seq(&session, "continue");
    session.rewind(cont).unwrap();
    let history = session.history();
    assert_eq!(history.len(), 1);
    assert!(user_texts(&history)[0].contains("SUMMARY: the user said hello"));
}

/// The log tells of the messages that wait after a rewind: the ones added
/// after the rewound message are added again, and the ones that left the
/// queue after it are removed again.
#[tokio::test]
async fn waiting_messages_stay_as_they_are() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![
            Script::Hang,
            text("now done"),
            text("held done"),
            Script::Hang,
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let mut live = Vec::new();
    session.send_prompt(vec![ContentPart::text("a")]).unwrap();
    live.extend(
        collect_until(&mut sub, |e| {
            matches!(
                e,
                SessionEvent::StateChanged {
                    state: SessionState::Thinking
                }
            )
        })
        .await,
    );
    session
        .send_message(vec![ContentPart::text("held")], Delivery::Queue)
        .unwrap();
    session.interrupt();
    session.wait_idle().await;
    // "now" starts a turn; "held" waits for it, then has its own.
    session
        .send_message(vec![ContentPart::text("now")], Delivery::Queue)
        .unwrap();
    for _ in 0..2 {
        live.extend(
            collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await,
        );
    }
    session.wait_idle().await;
    assert!(session.pending().is_empty());

    // Rewound to before "now", "held" left the queue after it.
    let now = user_seq(&session, "now");
    session.rewind(now).unwrap();
    let log = session.subscribe(None).backlog;
    assert!(logged_pending(&log).is_empty(), "{log:?}");
    assert_eq!(logged_users(&log), ["a"]);

    // A message that waits after the rewind is still told of.
    session.send_prompt(vec![ContentPart::text("b")]).unwrap();
    collect_until(&mut sub, |e| {
        matches!(
            e,
            SessionEvent::StateChanged {
                state: SessionState::Thinking
            }
        )
    })
    .await;
    session
        .send_message(vec![ContentPart::text("later")], Delivery::Queue)
        .unwrap();
    session.interrupt();
    session.wait_idle().await;
    let b = user_seq(&session, "b");
    session.rewind(b).unwrap();
    let log = session.subscribe(None).backlog;
    let pending = session.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(logged_pending(&log), pending);
    // A client that saw every event agrees.
    let mut shown = Vec::new();
    while let Ok(event) = sub.live.try_recv() {
        live.push(event.event);
    }
    for event in &live {
        apply_pending(&mut shown, event);
    }
    assert_eq!(shown, pending);
}
