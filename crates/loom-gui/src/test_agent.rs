//! A real agent, in this process, for tests that need the GUI to talk to
//! one: it serves a workspace in a temporary directory over an in-memory
//! connection.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, AppContext, Entity, TestAppContext, VisualTestContext};
use loom_agent::server::{ServerContext, serve_stream};
use loom_agent_core::{Agent, AgentOptions};
use loom_client::proto::{AgentInfo, InstanceId};
use loom_client::{Client, memory_pair};
use loom_config::{AgentConfig, Dirs, HostEntry};

use crate::backend::Backend;
use crate::state::AgentConnection;

pub struct TestAgent {
    pub dir: tempfile::TempDir,
    pub workspace: PathBuf,
    pub backend: Backend,
    pub info: AgentInfo,
    agent: Agent,
}

impl TestAgent {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let dirs = Dirs {
            home: root.clone(),
            config_home: root.join("config"),
            data_home: root.join("data"),
            state_home: root.join("state"),
            runtime_dir: None,
        };
        let backend = Backend::new().unwrap();
        let options = AgentOptions::new(&workspace, AgentConfig::default(), dirs);
        let agent = futures::executor::block_on(backend.run(async move { Agent::new(options) }))
            .unwrap()
            .unwrap();
        let info = AgentInfo {
            instance: InstanceId("test-instance".into()),
            workspace: workspace.clone(),
            pid: std::process::id(),
            started_at: 0,
            version: "test".into(),
            default_model: None,
        };
        Self {
            dir,
            workspace,
            backend,
            info,
            agent,
        }
    }

    /// A connection to the agent, the way the workspace makes one.
    pub fn connect(&self, cx: &mut TestAppContext) -> Entity<AgentConnection> {
        self.connect_as(
            HostEntry::Local {
                name: "local".into(),
            },
            cx,
        )
    }

    /// A connection that the GUI takes for one to `host`; it still reaches
    /// the agent directly.
    pub fn connect_as(&self, host: HostEntry, cx: &mut TestAppContext) -> Entity<AgentConnection> {
        // Replies from the agent wake the app from the backend's threads, and
        // the connection asks for one as soon as it exists.
        cx.executor().allow_parking();
        let context = ServerContext::new(self.agent.clone(), self.info.clone());
        let (client, notifications) = futures::executor::block_on(self.backend.run(async move {
            let (client_side, server_side) = memory_pair();
            tokio::spawn(serve_stream(context, server_side));
            Client::connect(client_side, "test").await
        }))
        .unwrap()
        .unwrap();
        let backend = self.backend.clone();
        cx.new(|cx| AgentConnection::new(host, client, notifications, backend, cx))
    }

    pub fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        std::fs::write(self.workspace.join(path), content).unwrap();
    }

    pub fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.workspace.join(path)).unwrap()
    }
}

/// Runs the app until `done` holds, letting replies from the agent, which
/// arrive on other threads, come in. Panics after ten seconds.
pub fn wait_until(cx: &mut VisualTestContext, mut done: impl FnMut(&mut App) -> bool) {
    cx.executor().allow_parking();
    for _ in 0..1000 {
        cx.run_until_parked();
        if cx.update(|_, cx| done(cx)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the agent");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection asks the agent for its sessions as soon as it exists. The
    /// reply wakes the app from the backend's thread, which a test must allow
    /// before it waits for anything, or gpui fails it as nondeterministic.
    #[gpui::test]
    fn replies_may_arrive_before_the_test_waits(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let conn = agent.connect(cx);
        // The refresh is waiting on the backend; let its reply arrive now.
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(200));
        cx.run_until_parked();
        assert!(conn.read_with(cx, |conn, _| conn.sessions.is_empty()));
    }
}
