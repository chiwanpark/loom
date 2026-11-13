mod common;

use common::{Harness, MODEL_REF, calls, collect_until, run_turn, test_config, text};
use loom_agent_core::{AgentNotice, CoreError};
use loom_proto::{SessionEvent, Task, TaskChange, TaskStatus, ToolResult};
use serde_json::json;

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

fn add(text: &str) -> TaskChange {
    TaskChange::Add {
        text: text.into(),
        status: TaskStatus::Created,
    }
}

fn summary(tasks: &[Task]) -> Vec<(u32, &str, TaskStatus)> {
    tasks
        .iter()
        .map(|t| (t.id, t.text.as_str(), t.status))
        .collect()
}

/// Under a policy that asks for everything else, the agent reads and changes
/// the task list without asking, and the results are not file changes.
#[tokio::test]
async fn the_agent_reads_and_updates_tasks_without_asking() {
    let h = Harness::new(
        test_config(loom_config::PolicyAction::Ask),
        vec![
            calls(&[
                ("r1", "tasks_read", json!({})),
                (
                    "u",
                    "tasks_update",
                    json!({"changes": [
                        {"action": "update", "id": 1, "status": "done"},
                        {"action": "add", "text": "test it", "status": "in_progress"},
                    ]}),
                ),
                (
                    "bad",
                    "tasks_update",
                    json!({"changes": [
                        {"action": "remove", "id": 2},
                        {"action": "remove", "id": 9},
                    ]}),
                ),
                ("r2", "tasks_read", json!({})),
            ]),
            text("ok"),
        ],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    session.update_tasks(vec![add("build it")]).unwrap();
    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "go").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );
    let results = results(&events);
    assert_eq!(result(&results, "r1").content, "#1 [created] build it\n");
    let updated = result(&results, "u");
    assert!(!updated.is_error, "{}", updated.content);
    assert_eq!(updated.details, None);
    let bad = result(&results, "bad");
    assert!(bad.is_error);
    assert!(bad.content.contains("no task #9"), "{}", bad.content);
    assert_eq!(
        result(&results, "r2").content,
        "#1 [done] build it\n#2 [in_progress] test it\n"
    );
    assert_eq!(
        summary(&session.tasks()),
        [
            (1, "build it", TaskStatus::Done),
            (2, "test it", TaskStatus::InProgress)
        ]
    );
}

#[tokio::test]
async fn tasks_outlast_compaction_clearing_and_a_restart() {
    let config = test_config(loom_config::PolicyAction::Allow);
    let h = Harness::new(
        config.clone(),
        vec![text("one"), text("two"), text("SUMMARY: two turns")],
    );
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    assert!(session.tasks().is_empty());
    session.update_tasks(vec![add("a"), add("b")]).unwrap();
    let mut sub = session.subscribe(None);
    run_turn(&session, &mut sub, "first").await;
    run_turn(&session, &mut sub, "second").await;
    session.compact().unwrap();
    collect_until(&mut sub, |e| {
        matches!(e, SessionEvent::ContextCompacted { .. })
    })
    .await;
    session.wait_idle().await;
    session.clear().unwrap();
    session
        .update_tasks(vec![TaskChange::Remove { id: 1 }])
        .unwrap();
    let id = session.id().clone();
    h.agent.shutdown().await;
    drop(session);

    let (agent, _) = h.reopen(config, vec![]);
    let restored = agent.session(&id).unwrap();
    assert_eq!(summary(&restored.tasks()), [(2, "b", TaskStatus::Created)]);
    // Numbers stay unique after a restart too.
    let list = restored.update_tasks(vec![add("c")]).unwrap();
    assert_eq!(list.tasks.last().unwrap().id, 3);
}

#[tokio::test]
async fn the_task_list_has_a_limit() {
    let mut config = test_config(loom_config::PolicyAction::Allow);
    config.tools.tasks.max_items = 2;
    let h = Harness::new(config, vec![]);
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let list = session.update_tasks(vec![add("a"), add("b")]).unwrap();
    assert_eq!(list.max_tasks, 2);
    let err = session.update_tasks(vec![add("c")]).unwrap_err();
    assert!(matches!(err, CoreError::TooManyTasks { max: 2 }), "{err}");
    assert!(err.to_string().contains("tools.tasks.max_items"));
    // Removing first makes room within the same update.
    session
        .update_tasks(vec![TaskChange::Remove { id: 1 }, add("c")])
        .unwrap();
    assert_eq!(session.tasks().len(), 2);
}

/// Clients hear of every change, and of nothing else.
#[tokio::test]
async fn task_changes_are_announced() {
    let h = Harness::new(test_config(loom_config::PolicyAction::Allow), vec![]);
    let session = h.agent.create_session(Some(MODEL_REF)).unwrap();
    let id = session.id().clone();
    let mut notices = h.agent.notices();
    let mut changes = move || {
        let mut lists = Vec::new();
        while let Ok(notice) = notices.try_recv() {
            if let AgentNotice::TasksChanged { session, tasks } = notice {
                assert_eq!(session, id);
                lists.push(tasks.len());
            }
        }
        lists
    };
    session.update_tasks(vec![add("a")]).unwrap();
    session.update_tasks(vec![add("b")]).unwrap();
    assert_eq!(changes(), [1, 2]);
    // A failed or empty update changes nothing.
    assert!(
        session
            .update_tasks(vec![TaskChange::Remove { id: 7 }])
            .is_err()
    );
    session
        .update_tasks(vec![TaskChange::Update {
            id: 1,
            text: Some("a".into()),
            status: None,
        }])
        .unwrap();
    assert_eq!(changes(), Vec::<usize>::new());

    h.agent.delete_session(session.id()).unwrap();
    session.update_tasks(vec![add("late")]).unwrap();
    assert_eq!(changes(), Vec::<usize>::new());
}
