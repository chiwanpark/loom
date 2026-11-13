mod common;

use std::path::Path;
use std::time::Duration;

use common::{Harness, calls, outcome, run_turn, text};
use loom_agent_core::{AgentNotice, CoreError, watch_config};
use loom_config::{AgentConfig, PolicyAction};
use loom_proto::{ConfigReload, ErrorCode, SessionEvent, TurnOutcome};
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

/// An agent that knows the file its configuration came from.
fn agent_with_file(h: &Harness, path: &Path) -> loom_agent_core::Agent {
    let config = AgentConfig::load(path, &h.dirs).unwrap();
    let mut options = loom_agent_core::AgentOptions::new(&h.workspace, config, h.dirs.clone());
    options
        .providers
        .push(common::ScriptedProvider::new(vec![]));
    options.config_path = Some(path.to_path_buf());
    loom_agent_core::Agent::new(options).unwrap()
}

#[tokio::test]
async fn reloads_config_on_request_and_reports_the_outcome() {
    let probe = tempfile::tempdir().unwrap();
    let dirs = common::dirs_in(probe.path());
    let h = Harness::new(
        AgentConfig::parse(BASE, "agent.yaml", &dirs).unwrap(),
        vec![],
    );
    let path = h.dir.path().join("agent.yaml");
    std::fs::write(&path, BASE).unwrap();
    let agent = agent_with_file(&h, &path);
    let mut notices = agent.notices();

    assert_eq!(
        agent.reload_config().await.unwrap(),
        ConfigReload::Unchanged
    );

    std::fs::write(&path, BASE.replace("default: ask", "default: allow")).unwrap();
    assert_eq!(
        agent.reload_config().await.unwrap(),
        ConfigReload::Applied {
            restart_required: vec![]
        }
    );
    assert_eq!(agent.config().policy.default, PolicyAction::Allow);
    assert_eq!(
        next_notice(&mut notices).await,
        AgentNotice::ConfigReloaded {
            restart_required: vec![]
        }
    );

    // A rejected file leaves the configuration as it was, and says why.
    std::fs::write(&path, format!("{BASE}bogus: 1\n")).unwrap();
    let err = agent.reload_config().await.unwrap_err();
    assert!(matches!(err, CoreError::ConfigRejected(_)), "{err}");
    assert!(err.to_string().contains("bogus"), "{err}");
    assert_eq!(
        loom_agent_core::convert::api_error(&err).code,
        ErrorCode::Config
    );
    assert!(matches!(
        next_notice(&mut notices).await,
        AgentNotice::ConfigRejected { message } if message.contains("bogus")
    ));
    assert_eq!(agent.config().policy.default, PolicyAction::Allow);

    // Sections that need a restart are named, and keep their settings.
    let socket = h.dir.path().join("sockets");
    std::fs::write(
        &path,
        format!(
            "{}socket:\n  dir: {}\n",
            BASE.replace("default: ask", "default: allow"),
            socket.display()
        ),
    )
    .unwrap();
    assert_eq!(
        agent.reload_config().await.unwrap(),
        ConfigReload::Applied {
            restart_required: vec!["socket".into()]
        }
    );
    assert_ne!(agent.config().socket.dir.as_deref(), Some(socket.as_path()));
}

#[tokio::test]
async fn an_agent_without_a_file_cannot_reload() {
    let h = Harness::new(AgentConfig::default(), vec![]);
    let err = h.agent.reload_config().await.unwrap_err();
    assert!(matches!(err, CoreError::NoConfigFile), "{err}");
    assert_eq!(
        loom_agent_core::convert::api_error(&err).code,
        ErrorCode::Unsupported
    );
}
