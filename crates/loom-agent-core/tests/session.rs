mod common;

use std::time::Duration;

use common::{
    Harness, MODEL_REF, Script, calls, collect_until, next_event, outcome, reasoned, run_turn,
    test_config, text,
};
use loom_agent_core::CoreError;
use loom_config::{AgentConfig, PolicyAction, PolicyRule};
use loom_llm::{AssistantContent, Message};
use loom_proto::{ContentPart, Decision, ReasoningEffort, SessionEvent, SessionState, TurnOutcome};
use serde_json::json;

fn requested_tool_names(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCallRequested { call } => Some(call.name.clone()),
            _ => None,
        })
        .collect()
}

fn finished_results(events: &[SessionEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ToolCallFinished { result, .. } => {
                Some((result.content.clone(), result.is_error))
            }
            _ => None,
        })
        .collect()
}

fn model_changes(events: &[SessionEvent]) -> Vec<(String, Option<ReasoningEffort>)> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::ModelChanged { model, reasoning } => Some((model.clone(), *reasoning)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_change_of_model_or_effort_is_reported_before_the_next_response() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![
            text("one"),
            calls(&[("c1", "read", json!({"path": "missing.txt"}))]),
            text("two"),
            text("three"),
            text("four"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    // The first response has nothing to differ from.
    let events = run_turn(&session, &mut sub, "one").await;
    assert_eq!(model_changes(&events), []);

    // Reported once for a turn of two responses, before the first.
    session.set_reasoning(loom_llm::ReasoningEffort::High);
    let events = run_turn(&session, &mut sub, "two").await;
    assert_eq!(
        model_changes(&events),
        [(MODEL_REF.to_owned(), Some(ReasoningEffort::High))]
    );
    let changed = events
        .iter()
        .position(|e| matches!(e, SessionEvent::ModelChanged { .. }));
    let answered = events
        .iter()
        .position(|e| matches!(e, SessionEvent::AssistantMessage { .. }));
    assert!(changed < answered);

    let events = run_turn(&session, &mut sub, "three").await;
    assert_eq!(model_changes(&events), []);

    session.set_model("scripted/other").unwrap();
    session.set_reasoning(loom_llm::ReasoningEffort::Low);
    let events = run_turn(&session, &mut sub, "four").await;
    assert_eq!(
        model_changes(&events),
        [("scripted/other".to_owned(), Some(ReasoningEffort::Low))]
    );
}

#[tokio::test]
async fn a_new_session_takes_the_configured_reasoning_effort() {
    let mut config = test_config(PolicyAction::Allow);
    config.llm.default_reasoning_effort = Some(loom_config::ReasoningEffort::High);
    let h = Harness::new(config, vec![text("one"), text("two")]);
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    assert_eq!(
        session.info().reasoning,
        Some(loom_proto::ReasoningEffort::High)
    );
    run_turn(&session, &mut sub, "one").await;
    session.set_reasoning(loom_llm::ReasoningEffort::Low);
    run_turn(&session, &mut sub, "two").await;

    let efforts: Vec<_> = h.provider.requests().iter().map(|r| r.reasoning).collect();
    assert_eq!(
        efforts,
        [
            Some(loom_llm::ReasoningEffort::High),
            Some(loom_llm::ReasoningEffort::Low),
        ]
    );
    assert_eq!(
        session.info().reasoning,
        Some(loom_proto::ReasoningEffort::Low)
    );
}

#[tokio::test]
async fn text_turn_streams_and_builds_request() {
    let h = Harness::new(test_config(PolicyAction::Ask), vec![text("Hello there")]);
    std::fs::write(h.workspace.join("AGENTS.md"), "Always answer in haiku.").unwrap();
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "hi").await;

    assert!(matches!(events[0], SessionEvent::UserMessage { .. }));
    assert_eq!(
        events[1],
        SessionEvent::StateChanged {
            state: SessionState::Thinking
        }
    );
    let deltas: String = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, "Hello there");
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::AssistantMessage { text, model, .. }
            if text == "Hello there" && model == MODEL_REF
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::Usage { usage } if usage.total == 13
    )));
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert_eq!(
        next_event(&mut sub).await,
        SessionEvent::StateChanged {
            state: SessionState::Completed
        }
    );

    let request = &h.provider.requests()[0];
    assert_eq!(request.model, "test-model");
    let system = request.system.as_deref().unwrap();
    assert!(system.contains("You are Loom"));
    assert!(system.contains("Always answer in haiku."));
    assert!(system.contains(&h.agent.workspace_root().display().to_string()));
    let names: Vec<_> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "edit",
            "notes_edit",
            "notes_read",
            "notes_write",
            "read",
            "shell",
            "tasks_read",
            "tasks_update",
            "write"
        ]
    );
    assert!(system.contains("# Notes and tasks"));
    assert_eq!(request.messages, vec![Message::user_text("hi")]);
    assert_eq!(request.session_id.as_deref(), Some(session.id().0.as_str()));

    let replay: Vec<_> = session.subscribe(None).backlog;
    assert!(
        replay
            .iter()
            .all(|e| !matches!(e.event, SessionEvent::AssistantDelta { .. }))
    );
}

#[tokio::test]
async fn configured_system_prompt_replaces_builtin_instructions() {
    let mut config = test_config(PolicyAction::Ask);
    config.instructions.system_prompt = Some("You are a terse reviewer.".into());
    let h = Harness::new(config, vec![text("ok")]);
    std::fs::write(h.workspace.join("AGENTS.md"), "Always answer in haiku.").unwrap();
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hi").await;

    let request = &h.provider.requests()[0];
    let system = request.system.as_deref().unwrap();
    assert!(system.starts_with("You are a terse reviewer."), "{system}");
    assert!(!system.contains("You are Loom"), "{system}");
    assert!(system.contains("Always answer in haiku."));
    assert!(system.contains("# Environment"));
}

#[tokio::test]
async fn system_prompt_is_what_the_next_model_call_sends() {
    let mut config = test_config(PolicyAction::Ask);
    config.instructions.max_bytes = "8B".parse().unwrap();
    let h = Harness::new(config, vec![text("ok")]);
    std::fs::write(h.workspace.join("AGENTS.md"), "Always answer in haiku.").unwrap();
    let session = h.agent.create_session(None).unwrap();

    let prompt = session.system_prompt().await;
    assert!(prompt.text.contains("Always a"), "{}", prompt.text);
    assert!(!prompt.text.contains("haiku"), "{}", prompt.text);
    assert_eq!(prompt.warnings.len(), 1, "{:?}", prompt.warnings);
    assert!(prompt.warnings[0].contains("truncated"));

    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "hi").await;
    let request = &h.provider.requests()[0];
    assert_eq!(request.system.as_deref(), Some(prompt.text.as_str()));
    assert_eq!(prompt.tools.len(), request.tools.len());
    for (shown, sent) in prompt.tools.iter().zip(&request.tools) {
        assert_eq!(shown.name, sent.name);
        assert_eq!(shown.description, sent.description);
        let parameters: serde_json::Value = serde_json::from_str(&shown.parameters).unwrap();
        assert_eq!(parameters, sent.parameters, "{}", shown.name);
    }
}

#[tokio::test]
async fn skills_are_listed_and_loaded_and_their_files_read() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().canonicalize().unwrap().join("pdf");
    let reference = skill_dir.join("REFERENCE.md");
    let mut config = test_config(PolicyAction::Ask);
    // The user's skills are outside the workspace.
    config.skills.paths = vec![skill_dir.clone()];
    let h = Harness::new(
        config,
        vec![
            calls(&[("c1", "skill", json!({"name": "pdf"}))]),
            calls(&[("c2", "read", json!({"path": reference}))]),
            calls(&[("c3", "read", json!({"path": dir.path().join("secret.txt")}))]),
            text("done"),
        ],
    );
    std::fs::write(dir.path().join("secret.txt"), "secret").unwrap();
    let session = h.agent.create_session(None).unwrap();
    let tools = |prompt: &loom_proto::SystemPrompt| -> Vec<String> {
        prompt.tools.iter().map(|t| t.name.clone()).collect()
    };
    let prompt = session.system_prompt().await;
    assert!(!prompt.text.contains("# Skills"), "{}", prompt.text);
    assert!(!tools(&prompt).contains(&"skill".to_owned()));

    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: pdf\ndescription: Work with PDF files.\n---\n\nUse pdftotext.\n",
    )
    .unwrap();
    std::fs::write(&reference, "PDF REFERENCE\n").unwrap();
    let prompt = session.system_prompt().await;
    assert!(
        prompt.text.contains("# Skills") && prompt.text.contains("- pdf: Work with PDF files."),
        "{}",
        prompt.text
    );
    assert!(tools(&prompt).contains(&"skill".to_owned()));

    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "read the PDF").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    // No approval was asked: the default rule allows read, and skill is allowed.
    let results = finished_results(&events);
    assert_eq!(results.len(), 3);
    let (content, is_error) = &results[0];
    assert!(!is_error, "{content}");
    assert!(
        content.starts_with("<skill_content name=\"pdf\">\nUse pdftotext.\n"),
        "{content}"
    );
    assert!(content.contains(&format!("Skill directory: {}", skill_dir.display())));
    assert_eq!(results[1], ("PDF REFERENCE\n".to_owned(), false));
    let (content, is_error) = &results[2];
    assert!(
        *is_error && content.contains("outside the workspace"),
        "{content}"
    );
}

#[tokio::test]
async fn allowed_tool_calls_run_and_feed_results_back() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![
            calls(&[(
                "c1",
                "write",
                json!({"path": "notes.txt", "content": "hello"}),
            )]),
            calls(&[("c2", "read", json!({"path": "notes.txt"}))]),
            text("done"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "write a note").await;

    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert_eq!(requested_tool_names(&events), ["write", "read"]);
    let results = finished_results(&events);
    assert!(!results[0].1);
    assert_eq!(results[1], ("hello".to_owned(), false));
    assert_eq!(
        std::fs::read_to_string(h.workspace.join("notes.txt")).unwrap(),
        "hello"
    );
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    let last = &requests[2].messages;
    assert_eq!(last.len(), 5);
    assert!(matches!(
        &last[3],
        Message::Assistant(m) if matches!(&m.content[0], AssistantContent::ToolCall(c) if c.call_id == "c2")
    ));
    assert!(matches!(
        &last[4],
        Message::ToolResult { call_id, is_error: false, .. } if call_id == "c2"
    ));
}

#[tokio::test]
async fn allow_asks_for_harmful_calls_and_allow_harmful_does_not() {
    let stash = || calls(&[("c1", "shell", json!({"command": "git stash clear"}))]);
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![stash(), text("denied"), stash(), text("ran")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![ContentPart::text("clear")])
        .unwrap();
    let events = collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    let Some(SessionEvent::ApprovalRequired { reason, .. }) = events.last() else {
        panic!("expected an approval request");
    };
    assert!(
        reason.contains(loom_agent_core::harmful::POSSIBLY_HARMFUL),
        "{reason}"
    );
    assert!(reason.contains("git stash clear"), "{reason}");
    session.resolve_approval("c1", Decision::Deny).unwrap();
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert!(finished_results(&events)[0].0.contains("denied"));

    session.set_policy_default(PolicyAction::AllowHarmful);
    let events = run_turn(&session, &mut sub, "clear").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    assert!(!finished_results(&events)[0].0.contains("denied"));
}

#[tokio::test]
async fn the_comment_guard_follows_the_session_setting() {
    let write = || {
        calls(&[(
            "c1",
            "write",
            json!({"path": "a.rs", "content": "// Why.\nfn a() {}\n"}),
        )])
    };
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![write(), text("blocked"), write(), text("written")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let prompt = || {
        h.provider
            .requests()
            .last()
            .unwrap()
            .system
            .clone()
            .unwrap_or_default()
    };

    let events = run_turn(&session, &mut sub, "write").await;
    let results = finished_results(&events);
    assert!(results[0].1, "{}", results[0].0);
    assert!(results[0].0.contains("a.rs:1: // Why."), "{}", results[0].0);
    assert!(!h.workspace.join("a.rs").exists());
    assert!(prompt().contains("# Comments"));
    assert!(session.info().comment_guard);

    session.set_comment_guard(false);
    assert!(!session.info().comment_guard);
    let events = run_turn(&session, &mut sub, "write").await;
    assert!(!finished_results(&events)[0].1);
    assert!(h.workspace.join("a.rs").exists());
    assert!(!prompt().contains("# Comments"));
}

// The model thinks until it streams reasoning or an answer, responds while
// it streams text or tool calls, and the turn ends completed.
#[tokio::test]
async fn states_follow_the_model_and_the_tools() {
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![
            calls(&[("c1", "write", json!({"path": "a.txt", "content": "A"}))]),
            reasoned("the file is written", "Done"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![ContentPart::text("write a file")])
        .unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    session.resolve_approval("c1", Decision::Allow).unwrap();
    collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    next_event(&mut sub).await;
    let states: Vec<SessionState> = session
        .subscribe(None)
        .backlog
        .iter()
        .filter_map(|e| match e.event {
            SessionEvent::StateChanged { state } => Some(state),
            _ => None,
        })
        .collect();
    use SessionState::*;
    assert_eq!(
        states,
        [
            Thinking,
            Responding,
            WaitingApproval,
            RunningTool,
            Thinking,
            Responding,
            Completed
        ]
    );
}

#[tokio::test]
async fn approvals_gate_tool_calls() {
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![
            calls(&[("c1", "write", json!({"path": "a.txt", "content": "A"}))]),
            calls(&[("c2", "write", json!({"path": "b.txt", "content": "B"}))]),
            calls(&[("c3", "write", json!({"path": "c.txt", "content": "C"}))]),
            text("finished"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session
        .send_prompt(vec![ContentPart::text("write files")])
        .unwrap();

    let waiting = |e: &SessionEvent| matches!(e, SessionEvent::ApprovalRequired { .. });
    let events = collect_until(&mut sub, waiting).await;
    assert!(events.contains(&SessionEvent::StateChanged {
        state: SessionState::WaitingApproval
    }));
    assert!(matches!(
        session.resolve_approval("nope", Decision::Allow),
        Err(CoreError::UnknownApproval(_))
    ));
    session.resolve_approval("c1", Decision::Deny).unwrap();
    assert!(session.resolve_approval("c1", Decision::Allow).is_err());

    collect_until(&mut sub, waiting).await;
    session
        .resolve_approval("c2", Decision::AllowForSession)
        .unwrap();

    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert!(!events.iter().any(waiting));
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert!(!h.workspace.join("a.txt").exists());
    assert!(h.workspace.join("b.txt").exists());
    assert!(h.workspace.join("c.txt").exists());

    let history = session.history();
    let denied = history.iter().find_map(|m| match m {
        Message::ToolResult {
            call_id,
            content,
            is_error: true,
            ..
        } if call_id == "c1" => Some(content.clone()),
        _ => None,
    });
    assert!(format!("{denied:?}").contains("denied"));
}

#[tokio::test]
async fn configured_deny_rules_block_without_asking() {
    let mut config = test_config(PolicyAction::Ask);
    config.policy.rules.push(PolicyRule {
        tool: "shell".into(),
        action: PolicyAction::Deny,
        command_prefix: vec!["sudo".into()],
    });
    let h = Harness::new(
        config,
        vec![
            calls(&[("c1", "shell", json!({"command": "sudo rm -rf /tmp/x"}))]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "clean up").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    let results = finished_results(&events);
    assert!(results[0].1);
    assert!(results[0].0.contains("denied by policy"));
}

#[tokio::test]
async fn interrupt_while_waiting_for_approval_keeps_history_valid() {
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![
            calls(&[
                ("c1", "shell", json!({"command": "echo one"})),
                ("c2", "shell", json!({"command": "echo two"})),
            ]),
            text("after"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session.send_prompt(vec![ContentPart::text("go")]).unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ApprovalRequired { .. })
    })
    .await;
    assert!(session.interrupt());
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Interrupted));

    let results: Vec<_> = session
        .history()
        .into_iter()
        .filter_map(|m| match m {
            Message::ToolResult { call_id, .. } => Some(call_id),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["c1", "c2"]);

    session.set_policy_default(PolicyAction::Allow);
    let events = run_turn(&session, &mut sub, "continue").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
}

#[tokio::test]
async fn interrupt_cancels_streaming_and_running_tools() {
    let h = Harness::new(test_config(PolicyAction::Allow), vec![Script::Hang]);
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session.send_prompt(vec![ContentPart::text("hi")]).unwrap();
    collect_until(&mut sub, |e| {
        matches!(
            e,
            SessionEvent::StateChanged {
                state: SessionState::Thinking
            }
        )
    })
    .await;
    assert!(matches!(
        session.send_prompt(vec![ContentPart::text("again")]),
        Err(CoreError::Busy)
    ));
    session.interrupt();
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Interrupted));

    h.provider.push(calls(&[(
        "c1",
        "shell",
        json!({"command": "echo started; sleep 30"}),
    )]));
    session.send_prompt(vec![ContentPart::text("run")]).unwrap();
    collect_until(&mut sub, |e| matches!(e, SessionEvent::ToolOutput { .. })).await;
    session.interrupt();
    let events = collect_until(&mut sub, |e| matches!(e, SessionEvent::TurnFinished { .. })).await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Interrupted));
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn provider_errors_fail_the_turn() {
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![Script::Fail("usage limit".into()), text("recovered")],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "hi").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Failed));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::Error { message } if message.contains("usage limit")
    )));
    let events = run_turn(&session, &mut sub, "again").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert_eq!(h.provider.requests()[1].messages.len(), 2);
}

#[tokio::test]
async fn web_search_is_offered_only_with_openai_codex_credentials() {
    // Signed out, a call to it is refused without asking for approval.
    let h = Harness::new(
        test_config(PolicyAction::Ask),
        vec![
            calls(&[("c1", "search_web", json!({"query": "tokio"}))]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "hi").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    let results = finished_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].1);
    assert!(
        results[0]
            .0
            .starts_with("search_web is unavailable: sign in to OpenAI Codex"),
        "{}",
        results[0].0
    );
    let offered = |request: &loom_llm::CompletionRequest| {
        request.tools.iter().any(|tool| tool.name == "search_web")
    };
    assert!(h.provider.requests().iter().all(|r| !offered(r)));

    // Once signed in, the next request offers it.
    let store = loom_llm::CredentialStore::in_dir(&loom_agent_core::data_dir(
        &test_config(PolicyAction::Ask),
        &h.dirs,
    ));
    store
        .update(|c| {
            c.set_section(
                "openai-codex",
                Some(json!({
                    "account_id": "a",
                    "access_token": "t",
                    "refresh_token": "r",
                    "expires_at": 0,
                    "saved_at": 0,
                })),
            )
        })
        .await
        .unwrap();
    h.provider.push(text("ok"));
    run_turn(&session, &mut sub, "again").await;
    assert!(offered(h.provider.requests().last().unwrap()));
}

#[tokio::test]
async fn unknown_tools_and_bad_arguments_are_reported_to_the_model() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![
            calls(&[
                ("c1", "teleport", json!({})),
                ("c2", "read", json!([1])),
                // Text a provider could not parse: a raw tab inside a string.
                ("c3", "read", json!("{\"path\": \"a\tb\"}")),
            ]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "hi").await;
    let results = finished_results(&events);
    assert!(results[0].0.contains("unknown tool"));
    assert!(results[1].0.contains("JSON object"));
    assert!(results[2].0.contains("not valid JSON"), "{}", results[2].0);
    assert!(
        results[2].0.contains("control character"),
        "{}",
        results[2].0
    );
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
}

#[tokio::test]
async fn shell_output_streams_and_is_compacted() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![
            calls(&[("c1", "shell", json!({"command": "printf 'a\\nb\\n'"}))]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "run").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolOutput { .. }))
    );
    assert!(events.contains(&SessionEvent::StateChanged {
        state: SessionState::RunningTool
    }));
    let replay = session.subscribe(None).backlog;
    assert!(
        replay
            .iter()
            .all(|e| !matches!(e.event, SessionEvent::ToolOutput { .. }))
    );
}

#[tokio::test]
async fn sessions_wait_for_conflicting_locks() {
    let h = Harness::new(
        test_config(PolicyAction::Allow),
        vec![calls(&[("s1", "shell", json!({"command": "sleep 0.5"}))])],
    );
    std::fs::write(h.workspace.join("f.txt"), "x").unwrap();
    let a = h.agent.create_session(None).unwrap();
    let b = h.agent.create_session(None).unwrap();
    let mut sub_a = a.subscribe(None);
    let mut sub_b = b.subscribe(None);
    a.send_prompt(vec![ContentPart::text("sleep")]).unwrap();
    collect_until(&mut sub_a, |e| {
        matches!(e, SessionEvent::ToolCallStarted { .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.provider
        .push(calls(&[("r1", "read", json!({"path": "f.txt"}))]));
    h.provider.push(text("done"));
    h.provider.push(text("done"));
    let events = run_turn(&b, &mut sub_b, "read").await;
    // Waiting for the lock is part of running the tool.
    let states: Vec<SessionState> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::StateChanged { state } => Some(*state),
            _ => None,
        })
        .collect();
    assert!(states.contains(&SessionState::RunningTool), "{states:?}");
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    collect_until(&mut sub_a, |e| {
        matches!(e, SessionEvent::TurnFinished { .. })
    })
    .await;
    assert_eq!(h.agent.list_sessions().len(), 2);
    assert!(h.agent.session(b.id()).is_ok());
}

#[tokio::test]
async fn prompt_validation() {
    let mut config = AgentConfig::default();
    config.llm.default_model = None;
    let h = Harness::new(config, vec![]);
    let session = h.agent.create_session(None).unwrap();
    assert_eq!(session.info().model, None);
    assert!(matches!(
        session.send_prompt(vec![ContentPart::text("hi")]),
        Err(CoreError::NoModel)
    ));
    assert!(matches!(
        session.send_prompt(vec![ContentPart::text("  ")]),
        Err(CoreError::EmptyPrompt)
    ));
    session.set_model(MODEL_REF).unwrap();
    assert!(session.set_model("bogus").is_err());
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "hi").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert!(h.agent.create_session(Some("nope/x")).is_err());
}

#[tokio::test]
async fn unknown_default_model_provider_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config(PolicyAction::Ask);
    config.llm.default_model = Some("missing/model".into());
    let options =
        loom_agent_core::AgentOptions::new(dir.path(), config, common::dirs_in(dir.path()));
    let err = loom_agent_core::Agent::new(options).err().unwrap();
    assert!(err.to_string().contains("missing"), "{err}");
}

#[tokio::test]
async fn a_turn_stops_at_the_round_limit_and_zero_means_none() {
    let round = || calls(&[("c", "tasks_read", json!({}))]);
    let mut config = test_config(PolicyAction::Allow);
    config.limits.max_rounds_per_turn = 3;
    let h = Harness::new(config.clone(), (0..3).map(|_| round()).collect());
    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "loop").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Failed));
    assert!(events.iter().any(|e| matches!(
        e,
        SessionEvent::Error { message }
            if message.contains("3 model calls") && message.contains("limits.max_rounds_per_turn")
    )));
    assert_eq!(h.provider.requests().len(), 3);

    // A reload applies to the next turn; without a limit it runs to the end.
    config.limits.max_rounds_per_turn = 0;
    h.agent.apply_config(config).unwrap();
    for _ in 0..4 {
        h.provider.push(round());
    }
    h.provider.push(text("done"));
    let events = run_turn(&session, &mut sub, "go on").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert_eq!(h.provider.requests().len(), 3 + 5);
}
