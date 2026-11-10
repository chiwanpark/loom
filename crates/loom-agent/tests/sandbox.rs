use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use loom_agent_core::{Agent, AgentOptions};
use loom_client::proto::{ContentPart, SessionEvent};
use loom_config::{AgentConfig, Dirs, PolicyAction, SandboxBackend};
use loom_llm::{
    AssistantContent, AssistantMessage, ChunkStream, CompletionChunk, CompletionRequest,
    LlmProvider, ProviderId, StopReason, ToolCall, Usage,
};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_loom-agent");

struct Scripted(Mutex<VecDeque<AssistantMessage>>);

fn message(content: Vec<AssistantContent>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        provider: ProviderId::from_static("scripted"),
        model: "m".into(),
        content,
        usage: Usage::default(),
        stop_reason: stop,
        response_id: None,
    }
}

#[async_trait]
impl LlmProvider for Scripted {
    fn id(&self) -> ProviderId {
        ProviderId::from_static("scripted")
    }

    async fn stream(&self, _request: CompletionRequest) -> loom_llm::Result<ChunkStream> {
        let next = self.0.lock().unwrap().pop_front().unwrap_or_else(|| {
            message(
                vec![AssistantContent::Text {
                    text: "done".into(),
                    provider_data: None,
                }],
                StopReason::Stop,
            )
        });
        Ok(futures::stream::iter(vec![Ok(CompletionChunk::Done(next))]).boxed())
    }
}

async fn run_shell(backend: SandboxBackend, network: bool, command: &str, root: &Path) -> String {
    let workspace = root.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let dirs = Dirs {
        home: root.to_path_buf(),
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
        runtime_dir: None,
    };
    let mut config = AgentConfig::default();
    config.llm.default_model = Some("scripted/m".into());
    config.policy.default = PolicyAction::Allow;
    config.sandbox.backend = backend;
    config.sandbox.network = network;
    let call = message(
        vec![AssistantContent::ToolCall(ToolCall {
            call_id: "c1".into(),
            name: "shell".into(),
            arguments: json!({ "command": command }),
            provider_data: None,
        })],
        StopReason::ToolUse,
    );
    let mut options = AgentOptions::new(&workspace, config, dirs);
    options
        .providers
        .push(Arc::new(Scripted(Mutex::new(vec![call].into()))));
    options.sandbox_exe = Some(BIN.into());
    options.persist = false;
    let agent = Agent::new(options).unwrap();
    let session = agent.create_session(None).unwrap();
    let mut sub = session.subscribe(None);
    session.send_prompt(vec![ContentPart::text("go")]).unwrap();
    let mut output = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), sub.live.recv())
            .await
            .unwrap()
            .unwrap()
            .event;
        match event {
            SessionEvent::ToolCallFinished { result, .. } => output = Some(result.content),
            SessionEvent::TurnFinished { .. } => break,
            _ => {}
        }
    }
    output.unwrap()
}

#[tokio::test]
async fn landlock_confines_shell_commands() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let command = format!(
        "echo in > inside.txt && echo ok; echo out > {}/outside.txt; echo done",
        root.display()
    );
    let output = run_shell(SandboxBackend::Landlock, false, &command, &root).await;
    assert!(output.contains("ok"), "{output}");
    assert!(output.contains("Permission denied"), "{output}");
    assert!(root.join("ws/inside.txt").exists());
    assert!(!root.join("outside.txt").exists());
}

#[tokio::test]
async fn bubblewrap_confines_shell_commands() {
    if loom_tools::sandbox::find_bwrap().is_none() {
        return;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let command = format!(
        "echo in > inside.txt && echo ok; echo out > {}/outside.txt; echo done",
        root.display()
    );
    let output = run_shell(SandboxBackend::Bubblewrap, true, &command, &root).await;
    assert!(output.contains("ok"), "{output}");
    assert!(output.contains("Read-only file system"), "{output}");
    assert!(root.join("ws/inside.txt").exists());
    assert!(!root.join("outside.txt").exists());
}
