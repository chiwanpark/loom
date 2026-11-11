mod common;

use common::{Harness, Script, calls, collect_until, run_turn, test_config, text};
use loom_config::PolicyAction;
use loom_llm::{InputContent, Message};
use loom_proto::{SessionEvent, TurnOutcome};
use serde_json::json;

fn with_usage(script: Script, input: u64) -> Script {
    match script {
        Script::Reply(mut message) => {
            message.usage.input = input;
            Script::Reply(message)
        }
        other => other,
    }
}

fn first_text(message: &Message) -> String {
    match message {
        Message::User { content } => content
            .iter()
            .find_map(|c| match c {
                InputContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn summarizes_earlier_turns_when_the_context_fills_up() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.window_tokens = Some(1000);
    config.context.compaction_threshold = 0.5;
    let h = Harness::new(
        config,
        vec![
            with_usage(text("first answer"), 900),
            text("SUMMARY: the user said hello"),
            text("second answer"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hello").await;
    let events = run_turn(&session, &mut sub, "continue").await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::ContextCompacted {
            removed_messages: 2,
            summarized: true
        }
    )));
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].tools.is_empty());
    assert!(requests[1].system.as_deref().unwrap().contains("summarize"));
    assert_eq!(requests[1].messages.len(), 3);
    let last = &requests[2].messages;
    assert_eq!(last.len(), 2);
    assert!(first_text(&last[0]).contains("SUMMARY: the user said hello"));
    assert_eq!(first_text(&last[1]), "continue");
    assert_eq!(session.history().len(), 3);
}

/// Without a configured window, the one the provider reports for the model
/// decides when to compact, and the model's capabilities reach the provider.
#[tokio::test]
async fn compacts_against_the_window_the_provider_reports() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.window_tokens = None;
    config.context.compaction_threshold = 0.5;
    let h = Harness::new(
        config,
        vec![
            with_usage(text("first answer"), 900),
            text("SUMMARY: the user said hello"),
            text("second answer"),
        ],
    );
    let mut model = loom_llm::ModelInfo::named(common::MODEL);
    model.capabilities.context_window = Some(1000);
    model.capabilities.max_output_tokens = Some(4096);
    *h.provider.models.lock().unwrap() = vec![model.clone()];
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hello").await;
    let events = run_turn(&session, &mut sub, "continue").await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::ContextCompacted {
                summarized: true,
                ..
            }
        )),
        "{events:?}"
    );
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request.capabilities, model.capabilities);
    }
}

/// The default window is far larger than this conversation, so nothing is
/// compacted when the provider reports no window.
#[tokio::test]
async fn an_unreported_window_falls_back_to_the_default() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.window_tokens = None;
    config.context.compaction_threshold = 0.5;
    let h = Harness::new(
        config,
        vec![with_usage(text("first answer"), 900), text("second answer")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hello").await;
    let events = run_turn(&session, &mut sub, "continue").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ContextCompacted { .. })),
        "{events:?}"
    );
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn cached_prompt_tokens_count_toward_compaction() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.window_tokens = Some(1000);
    config.context.compaction_threshold = 0.5;
    // Anthropic-style usage: almost the whole prompt is a cache read.
    let mut first = text("first answer");
    if let Script::Reply(message) = &mut first {
        message.usage.input = 3;
        message.usage.cache_read = 800;
        message.usage.output = 20;
    }
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
    let events = run_turn(&session, &mut sub, "continue").await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::ContextCompacted {
                summarized: true,
                ..
            }
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn trims_old_tool_output_inside_one_long_turn() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.window_tokens = Some(1000);
    config.context.compaction_threshold = 0.5;
    let workspace_file = "big.txt";
    let h = Harness::new(config, vec![]);
    std::fs::write(h.workspace.join(workspace_file), "line\n".repeat(400)).unwrap();
    let mut scripts = Vec::new();
    for i in 0..6 {
        scripts.push(calls(&[(
            &format!("c{i}"),
            "read",
            json!({ "path": workspace_file }),
        )]));
    }
    scripts.push(text("done"));
    let (agent, provider) = h.reopen(h.agent.config().as_ref().clone(), scripts);
    let session = agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![loom_proto::ContentPart::text("read it many times")])
        .unwrap();
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert_eq!(common::outcome(&events), Some(TurnOutcome::Completed));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::ContextCompacted {
            summarized: false,
            ..
        }
    )));
    let last = provider.requests().pop().unwrap();
    let elided = last
        .messages
        .iter()
        .filter(|m| match m {
            Message::ToolResult { content, .. } => matches!(
                content.as_slice(),
                [InputContent::Text { text }] if text.contains("removed to save context")
            ),
            _ => false,
        })
        .count();
    assert!(elided >= 1, "{elided}");
}

#[tokio::test]
async fn compacts_on_request_below_the_threshold() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![
            text("first answer"),
            text("second answer"),
            text("SUMMARY: hello then continue"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let err = session.compact().unwrap_err();
    assert!(err.to_string().contains("nothing to compact"), "{err}");

    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hello").await;
    run_turn(&session, &mut sub, "continue").await;
    session.compact().unwrap();
    let mut events = collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ContextCompacted { .. })
    })
    .await;
    events.extend(
        collect_until(&mut sub, |e| {
            matches!(
                e,
                SessionEvent::StateChanged {
                    state: loom_proto::SessionState::Idle
                }
            )
        })
        .await,
    );
    session.wait_idle().await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::ContextCompacted {
                removed_messages: 2,
                summarized: true
            }
        )),
        "{events:?}"
    );
    // Manual compaction is not a turn.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnFinished { .. }))
    );
    let history = session.history();
    assert!(first_text(&history[0]).contains("SUMMARY: hello then continue"));
    assert_eq!(first_text(&history[1]), "continue");
    assert!(!session.is_busy());
    let requests = h.provider.requests();
    assert!(requests[2].system.as_deref().unwrap().contains("summarize"));
}

#[tokio::test]
async fn manual_compaction_can_be_interrupted() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![text("first"), text("second"), common::Script::Hang],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "one").await;
    run_turn(&session, &mut sub, "two").await;
    let before = session.history();
    session.compact().unwrap();
    assert!(session.is_busy());
    assert!(
        session
            .send_prompt(vec![loom_proto::ContentPart::text("x")])
            .is_err()
    );
    assert!(session.compact().is_err());
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(session.interrupt());
    session.wait_idle().await;
    assert!(!session.is_busy());
    assert_eq!(session.history(), before);
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::Warning { .. })).await;
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::Warning { message } if message.contains("interrupted"))
    ));
}
