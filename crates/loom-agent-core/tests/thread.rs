mod common;

use common::{Harness, MODEL_REF, collect_until, run_turn, test_config, text};
use loom_agent_core::CoreError;
use loom_config::{ByteSize, PolicyAction};
use loom_proto::SessionEvent;

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
