//! The environment of shell commands and the redaction of its secrets.

mod common;

use common::{Harness, MODEL_REF, calls, outcome, run_turn, test_config, text};
use loom_config::PolicyAction;
use loom_llm::{InputContent, Message};
use loom_proto::{SessionEvent, TurnOutcome};
use serde_json::json;

fn tool_results(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => content.iter().find_map(|part| match part {
                InputContent::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn commands_get_the_env_file_and_the_model_never_sees_its_values() {
    let mut config = test_config(PolicyAction::AllowHarmful);
    config.environment.redaction.allow = vec!["REGION".into()];
    let h = Harness::new(
        config,
        vec![
            calls(&[(
                "c1",
                "shell",
                json!({"command": "echo \"token=$LAKECTL_SECRET region=$REGION\"; printf %s \"$LAKECTL_SECRET\" | base64"}),
            )]),
            calls(&[("c2", "read", json!({"path": ".env"}))]),
            calls(&[(
                "c3",
                "write",
                json!({"path": ".env", "content": "LAKECTL_SECRET=x\n"}),
            )]),
            text("done"),
        ],
    );
    std::fs::write(
        h.workspace.join(".env"),
        "LAKECTL_SECRET=lakefs-secret-value-1\nREGION=eu-west-1\n",
    )
    .unwrap();
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "use lakectl").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));

    let results = tool_results(&session.history());
    assert!(
        results[0].contains("token=[REDACTED:LAKECTL_SECRET] region=eu-west-1"),
        "{}",
        results[0]
    );
    assert!(
        results[0].contains("[REDACTED:LAKECTL_SECRET]\n"),
        "base64: {}",
        results[0]
    );
    assert!(results[1].contains("LAKECTL_SECRET=[REDACTED:LAKECTL_SECRET]"));
    assert!(results[1].contains("REGION=eu-west-1"));
    assert!(results[2].contains("Ask the user"), "{}", results[2]);
    assert_eq!(
        std::fs::read_to_string(h.workspace.join(".env")).unwrap(),
        "LAKECTL_SECRET=lakefs-secret-value-1\nREGION=eu-west-1\n"
    );

    // Nothing that clients get or the store keeps holds the value.
    let log = format!("{:?}", session.subscribe(None).backlog);
    assert!(!log.contains("lakefs-secret-value-1"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolOutput { .. }))
    );
    let requests = h.provider.requests();
    let sent = format!("{requests:?}");
    assert!(!sent.contains("lakefs-secret-value-1"));
    let system = requests[0].system.as_deref().unwrap();
    assert!(system.contains("- Secret: `LAKECTL_SECRET`."), "{system}");
    assert!(system.contains("- Not secret: `REGION`."), "{system}");
    // The workspace is not a git repository, so there is no warning about it.
    let prompt = session.system_prompt().await;
    assert!(prompt.warnings.is_empty(), "{:?}", prompt.warnings);
}
