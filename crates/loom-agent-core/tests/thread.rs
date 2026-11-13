mod common;

use common::{Harness, MODEL_REF, calls, collect_until, run_turn, test_config, text};
use loom_agent_core::policy::DENIED_BY_POLICY;
use loom_agent_core::thread::version;
use loom_agent_core::{AgentNotice, CoreError};
use loom_config::{ByteSize, PolicyAction, PolicyRule};
use loom_llm::{InputContent, Message};
use loom_proto::{SessionEvent, ToolResult};
use serde_json::json;

/// The results of the turn's tool calls, by call ID.
fn results(events: &[SessionEvent]) -> Vec<(String, ToolResult)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCallFinished { call_id, result } => {
                Some((call_id.clone(), result.clone()))
            }
            _ => None,
        })
        .collect()
}

fn result<'a>(results: &'a [(String, ToolResult)], id: &str) -> &'a ToolResult {
    &results.iter().find(|(call, _)| call == id).unwrap().1
}

/// The text the model got back for `tool_name` in its latest request.
fn sent_result(h: &Harness, tool_name: &str) -> String {
    let request = h.provider.requests().pop().unwrap();
    request
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult {
                tool_name: name,
                content,
                ..
            } if name == tool_name => content.iter().find_map(|c| match c {
                InputContent::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn the_thread_outlasts_compaction_clearing_and_a_restart() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(
        config.clone(),
        vec![text("one"), text("two"), text("SUMMARY: two turns")],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    assert_eq!(session.thread(), "");
    session.set_thread("- plan".into()).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "first").await;
    session
        .set_thread("- plan\n- first step done".into())
        .unwrap();
    run_turn(&session, &mut sub, "second").await;

    session.compact().unwrap();
    collect_until(&mut sub, |e| {
        matches!(
            e,
            SessionEvent::ContextCompacted {
                summarized: true,
                ..
            }
        )
    })
    .await;
    session.wait_idle().await;
    assert_eq!(session.thread(), "- plan\n- first step done");

    session.clear().unwrap();
    assert!(session.history().is_empty());
    assert_eq!(session.thread(), "- plan\n- first step done");
    let id = session.id().clone();
    let other = h
        .agent
        .create_session(Some(MODEL_REF))
        .unwrap()
        .id()
        .clone();
    h.agent.shutdown().await;
    drop(session);

    let (agent, _) = h.reopen(config, vec![]);
    let restored = agent.session(&id).unwrap();
    assert_eq!(restored.thread(), "- plan\n- first step done");
    assert_eq!(agent.session(&other).unwrap().thread(), "");
}

#[tokio::test]
async fn the_thread_has_a_size_limit() {
    let mut config = test_config(PolicyAction::Allow);
    config.tools.thread.max_bytes = ByteSize::bytes(8);
    let h = Harness::new(config, vec![]);
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    session.set_thread("12345678".into()).unwrap();
    let err = session.set_thread("123456789".into()).unwrap_err();
    assert!(
        matches!(err, CoreError::ThreadTooLarge { size: 9, max: 8 }),
        "{err}"
    );
    assert!(err.to_string().contains("tools.thread.max_bytes"));
    assert_eq!(session.thread(), "12345678");
}

/// Under a policy that asks for everything else, the agent reads, edits, and
/// writes the thread without asking, and the results are not file changes.
#[tokio::test]
async fn the_agent_reads_edits_and_writes_the_thread_without_asking() {
    let config = test_config(PolicyAction::Ask);
    let h = Harness::new(
        config,
        vec![
            calls(&[
                ("r1", "thread_read", json!({})),
                (
                    "e",
                    "thread_edit",
                    json!({"edits": [{"old_text": "- todo", "new_text": "- done"}]}),
                ),
                ("r2", "thread_read", json!({})),
            ]),
            calls(&[("w", "thread_write", json!({"text": "rewritten"}))]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    session.set_thread("- plan\n- todo".into()).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "go").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    let results = results(&events);
    assert_eq!(results.len(), 4);
    for (id, result) in &results {
        assert!(!result.is_error, "{id}: {}", result.content);
        assert_eq!(result.details, None, "{id}");
    }
    assert_eq!(result(&results, "r1").content, "- plan\n- todo");
    assert_eq!(result(&results, "r2").content, "- plan\n- done");
    assert_eq!(session.thread(), "rewritten");
}

/// The agent may replace all of the thread only while its conversation holds
/// the thread as it is; editing a part works regardless.
#[tokio::test]
async fn writing_the_whole_thread_needs_it_read_first() {
    let config = test_config(PolicyAction::Allow);
    let write = |id: &str, text: &str| (id.to_owned(), json!({ "text": text }));
    let script = |calls_: Vec<(String, serde_json::Value)>| {
        let refs: Vec<(&str, &str, serde_json::Value)> = calls_
            .iter()
            .map(|(id, args)| {
                let tool = if id.starts_with('r') {
                    "thread_read"
                } else if id.starts_with('e') {
                    "thread_edit"
                } else {
                    "thread_write"
                };
                (id.as_str(), tool, args.clone())
            })
            .collect();
        calls(&refs)
    };
    let read = |id: &str| (id.to_owned(), json!({}));
    let h = Harness::new(
        config,
        vec![
            // An empty thread holds nothing to lose.
            script(vec![write("w1", "mine")]),
            text("ok"),
            script(vec![
                write("w2", "lost"),
                (
                    "e1".into(),
                    json!({"old_text": "user", "new_text": "the user's"}),
                ),
                // An edit shows the agent only what it replaced.
                write("w2b", "lost too"),
                read("r1"),
                write("w3", "after reading"),
            ]),
            text("ok"),
            script(vec![write("w4", "after clearing")]),
            text("ok"),
            script(vec![read("r2")]),
            text("ok"),
            text("SUMMARY"),
            script(vec![write("w5", "after compacting")]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    let first = results(&run_turn(&session, &mut sub, "one").await);
    assert!(!result(&first, "w1").is_error);
    assert_eq!(session.thread(), "mine");

    session.set_thread("user notes".into()).unwrap();
    let second = results(&run_turn(&session, &mut sub, "two").await);
    let refused = result(&second, "w2");
    assert!(refused.is_error);
    assert!(
        refused.content.contains("thread_read"),
        "{}",
        refused.content
    );
    assert!(!result(&second, "e1").is_error);
    assert!(result(&second, "w2b").is_error);
    assert_eq!(result(&second, "r1").content, "the user's notes");
    assert!(!result(&second, "w3").is_error);
    assert_eq!(session.thread(), "after reading");

    session.clear().unwrap();
    let third = results(&run_turn(&session, &mut sub, "three").await);
    assert!(result(&third, "w4").is_error);
    assert_eq!(session.thread(), "after reading");

    run_turn(&session, &mut sub, "four").await;
    session.compact().unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ContextCompacted { .. })
    })
    .await;
    session.wait_idle().await;
    let fifth = results(&run_turn(&session, &mut sub, "five").await);
    assert!(result(&fifth, "w5").is_error);
    assert_eq!(session.thread(), "after reading");
}

#[tokio::test]
async fn thread_changes_are_checked() {
    let mut config = test_config(PolicyAction::Allow);
    config.tools.thread.max_bytes = ByteSize::bytes(16);
    let h = Harness::new(
        config,
        vec![
            calls(&[
                (
                    "missing",
                    "thread_edit",
                    json!({"old_text": "absent", "new_text": "x"}),
                ),
                (
                    "large-edit",
                    "thread_edit",
                    json!({"old_text": "9", "new_text": "9abcdefghi"}),
                ),
                ("read", "thread_read", json!({})),
                (
                    "large-write",
                    "thread_write",
                    json!({"text": "x".repeat(17)}),
                ),
                ("bad", "thread_write", json!({"txt": "x"})),
            ]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    session.set_thread("0123456789".into()).unwrap();
    let mut sub = session.subscribe(None);
    let results = results(&run_turn(&session, &mut sub, "go").await);
    let missing = result(&results, "missing");
    assert!(missing.is_error);
    assert!(
        missing
            .content
            .contains("could not find old_text in the operation thread"),
        "{}",
        missing.content
    );
    for id in ["large-edit", "large-write"] {
        let large = result(&results, id);
        assert!(large.is_error, "{id}");
        assert!(
            large.content.contains("tools.thread.max_bytes"),
            "{id}: {}",
            large.content
        );
    }
    assert!(result(&results, "bad").is_error);
    assert_eq!(session.thread(), "0123456789");
}

#[tokio::test]
async fn a_deny_rule_still_blocks_thread_tools() {
    let mut config = test_config(PolicyAction::Allow);
    config.policy.rules.push(PolicyRule {
        tool: "thread_write".into(),
        action: PolicyAction::Deny,
        command_prefix: Vec::new(),
    });
    let h = Harness::new(
        config,
        vec![
            calls(&[("w", "thread_write", json!({"text": "x"}))]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    let results = results(&run_turn(&session, &mut sub, "go").await);
    let denied = result(&results, "w");
    assert!(denied.is_error);
    assert!(denied.content.starts_with(DENIED_BY_POLICY));
    assert_eq!(session.thread(), "");
}

/// A read returns the whole thread to the model, even where other tool output
/// is cut off, so writing it back loses nothing.
#[tokio::test]
async fn thread_reads_are_not_cut_off() {
    let mut config = test_config(PolicyAction::Allow);
    config.context.max_tool_output_tokens = 10;
    let long = format!("{}END", "x".repeat(100));
    let h = Harness::new(
        config,
        vec![
            calls(&[
                ("t", "thread_read", json!({})),
                ("f", "read", json!({"path": "long.txt"})),
            ]),
            text("ok"),
        ],
    );
    std::fs::write(h.workspace.join("long.txt"), &long).unwrap();
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    session.set_thread(long.clone()).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "go").await;
    assert_eq!(sent_result(&h, "thread_read"), long);
    assert!(!sent_result(&h, "read").contains("END"));
}

/// Clients hear of every change in order, and of nothing else.
#[tokio::test]
async fn thread_changes_are_announced() {
    let config = test_config(PolicyAction::Allow);
    let h = Harness::new(
        config,
        vec![
            calls(&[("r", "thread_read", json!({}))]),
            text("ok"),
            calls(&[("w", "thread_write", json!({"text": "by the agent"}))]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let id = session.id().clone();
    let mut notices = h.agent.notices();
    let mut changes = move || {
        let mut versions = Vec::new();
        while let Ok(notice) = notices.try_recv() {
            if let AgentNotice::ThreadChanged { session, version } = notice {
                assert_eq!(session, id);
                versions.push(version);
            }
        }
        versions
    };

    let empty = session.thread_content();
    assert_eq!(empty.version, version(""));
    // Any change makes another version, whitespace too.
    assert_ne!(version("a"), version("a\n"));
    let v1 = session.write_thread("a".into(), empty.version).unwrap();
    assert_eq!(v1, version("a"));
    session.set_thread("b".into()).unwrap();
    assert_eq!(changes(), [v1, version("b")]);

    let err = session.write_thread("c".into(), v1).unwrap_err();
    assert!(matches!(err, CoreError::ThreadChanged), "{err}");
    assert_eq!(session.thread(), "b");
    assert_eq!(changes(), []);

    // Saving the same text changes nothing: no notice, and the agent, which
    // read it, may still replace it.
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "read").await;
    assert_eq!(
        session.write_thread("b".into(), version("b")).unwrap(),
        version("b")
    );
    assert_eq!(changes(), []);
    let results = results(&run_turn(&session, &mut sub, "write").await);
    assert!(!result(&results, "w").is_error);
    assert_eq!(changes(), [version("by the agent")]);

    h.agent.delete_session(session.id()).unwrap();
    session.set_thread("after deletion".into()).unwrap();
    assert_eq!(changes(), []);
}
