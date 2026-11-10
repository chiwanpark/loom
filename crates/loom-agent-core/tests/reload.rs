mod common;

use std::path::Path;
use std::time::Duration;

use common::{Harness, calls, outcome, run_turn, text};
use loom_agent_core::{AgentNotice, watch_config};
use loom_config::{AgentConfig, PolicyAction};
use loom_proto::{SessionEvent, TurnOutcome};
use serde_json::json;
use tokio::sync::broadcast;

const BASE: &str = "llm:\n  default_model: scripted/test-model\npolicy:\n  default: ask\n";

async fn next_notice(rx: &mut broadcast::Receiver<AgentNotice>) -> AgentNotice {
    loop {
        let notice = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for a notice")
            .unwrap();
        if !matches!(notice, AgentNotice::SessionUpdated(_)) {
            return notice;
        }
    }
}

fn replace(path: &Path, text: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[tokio::test]
async fn reloads_config_from_disk() {
    let probe = tempfile::tempdir().unwrap();
    let dirs = common::dirs_in(probe.path());
    let config = AgentConfig::parse(BASE, "agent.yaml", &dirs).unwrap();
    let h = Harness::new(config, vec![]);
    let path = h.dir.path().join("agent.yaml");
    std::fs::write(&path, BASE).unwrap();
    let mut notices = h.agent.notices();
    let _watcher = watch_config(h.agent.clone(), path.clone(), h.dirs.clone()).unwrap();

    std::fs::write(&path, BASE.replace("default: ask", "default: allow")).unwrap();
    assert_eq!(
        next_notice(&mut notices).await,
        AgentNotice::ConfigReloaded {
            restart_required: vec![]
        }
    );
    assert_eq!(h.agent.config().policy.default, PolicyAction::Allow);

    let session = h.agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    h.provider.push(calls(&[(
        "c1",
        "write",
        json!({"path": "x.txt", "content": "x"}),
    )]));
    h.provider.push(text("ok"));
    let events = run_turn(&session, &mut sub, "write").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ApprovalRequired { .. }))
    );

    std::fs::write(&path, "policy: [not, a, map\n").unwrap();
    assert!(matches!(
        next_notice(&mut notices).await,
        AgentNotice::ConfigRejected { .. }
    ));
    assert_eq!(h.agent.config().policy.default, PolicyAction::Allow);

    replace(
        &path,
        &format!(
            "{}socket:\n  dir: /tmp/loom-test-sockets\n",
            BASE.replace("default: ask", "default: deny")
        ),
    );
    assert_eq!(
        next_notice(&mut notices).await,
        AgentNotice::ConfigReloaded {
            restart_required: vec!["socket".into()]
        }
    );
    let config = h.agent.config();
    assert_eq!(config.policy.default, PolicyAction::Deny);
    assert_eq!(config.socket.dir, None);

    replace(
        &path,
        "llm:\n  default_model: scripted/test-model\n  providers:\n    missing-provider: {}\n",
    );
    let notice = next_notice(&mut notices).await;
    assert!(
        matches!(&notice, AgentNotice::ConfigRejected { message } if message.contains("missing-provider")),
        "{notice:?}"
    );
    assert_eq!(h.agent.config().policy.default, PolicyAction::Deny);
}
