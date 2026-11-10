use std::io::Write;
use std::path::PathBuf;

use loom_agent_core::{Agent, AgentOptions, watch_config};
use loom_config::{AgentConfig, CONFIG_ENV, Dirs};
use loom_llm::ReasoningEffort;
use loom_proto::{ContentPart, Decision, SessionEvent, SessionState};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

const USAGE: &str =
    "usage: chat [--workspace DIR] [--config FILE] [--model PROVIDER/MODEL] [--reasoning LEVEL]";

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == name)?;
    args.remove(pos);
    (pos < args.len()).then(|| args.remove(pos))
}

fn parse_reasoning(value: &str) -> AnyResult<ReasoningEffort> {
    Ok(match value {
        "none" => ReasoningEffort::None,
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::Xhigh,
        other => return Err(format!("unknown reasoning level {other}").into()),
    })
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let workspace = take_opt(&mut args, "--workspace")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let config_arg = take_opt(&mut args, "--config").map(PathBuf::from);
    let model = take_opt(&mut args, "--model");
    let reasoning = take_opt(&mut args, "--reasoning")
        .map(|r| parse_reasoning(&r))
        .transpose()?;
    if !args.is_empty() {
        return Err(USAGE.into());
    }

    let dirs = Dirs::from_env()?;
    let config_path = dirs.agent_config_path(config_arg.as_deref(), std::env::var_os(CONFIG_ENV));
    let config = AgentConfig::load(&config_path, &dirs)?;
    let agent = Agent::new(AgentOptions::new(&workspace, config, dirs.clone()))?;
    let _watcher = watch_config(agent.clone(), config_path.clone(), dirs).ok();
    let session = agent.create_session(model.as_deref())?;
    session.set_reasoning(reasoning.or(Some(ReasoningEffort::Low)));

    eprintln!("workspace: {}", agent.workspace_root().display());
    eprintln!("config: {}", config_path.display());
    eprintln!(
        "model: {}",
        session
            .info()
            .model
            .or_else(|| agent.config().llm.default_model.clone())
            .unwrap_or_else(|| "(none; pass --model)".into())
    );
    eprintln!("Type a prompt and press Enter. Ctrl-C interrupts a turn; Ctrl-D exits.\n");

    let (line_tx, mut lines) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = stdin.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut sub = session.subscribe(None);
    let mut pending_approval: Option<String> = None;
    prompt_marker();
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                let line = line.trim().to_owned();
                if let Some(call_id) = pending_approval.take() {
                    let decision = match line.as_str() {
                        "y" | "yes" => Decision::Allow,
                        "a" | "always" => Decision::AllowForSession,
                        _ => Decision::Deny,
                    };
                    session.resolve_approval(&call_id, decision)?;
                    continue;
                }
                if line.is_empty() {
                    prompt_marker();
                    continue;
                }
                if let Err(err) = session.send_prompt(vec![ContentPart::text(line)]) {
                    eprintln!("error: {err}");
                    prompt_marker();
                }
            }
            event = sub.live.recv() => {
                let Ok(event) = event else { continue };
                match event.event {
                    SessionEvent::ReasoningDelta { text } => eprint!("\x1b[2m{text}\x1b[0m"),
                    SessionEvent::AssistantDelta { text } => print!("{text}"),
                    SessionEvent::AssistantMessage { .. } => println!(),
                    SessionEvent::ToolCallRequested { call } => {
                        eprintln!("\x1b[36m[{}] {}\x1b[0m", call.name, call.arguments)
                    }
                    SessionEvent::ApprovalRequired { call_id, reason } => {
                        eprint!("\x1b[33mAllow this call? ({reason}) [y]es / [a]lways / [n]o: \x1b[0m");
                        pending_approval = Some(call_id);
                    }
                    SessionEvent::ToolOutput { chunk, .. } => {
                        eprint!("\x1b[2m{}\x1b[0m", String::from_utf8_lossy(&chunk.data))
                    }
                    SessionEvent::ToolCallFinished { result, .. } => {
                        let first = result.content.lines().next().unwrap_or_default();
                        let mark = if result.is_error { "failed" } else { "done" };
                        eprintln!("\x1b[36m[{mark}] {first}\x1b[0m");
                    }
                    SessionEvent::StateChanged { state: SessionState::WaitingLock } => {
                        eprintln!("[waiting for a workspace lock]")
                    }
                    SessionEvent::Usage { usage } => {
                        eprintln!("\x1b[2m[tokens: in {} (cached {}), out {}]\x1b[0m", usage.input, usage.cache_read, usage.output)
                    }
                    SessionEvent::Warning { message } => eprintln!("warning: {message}"),
                    SessionEvent::Error { message } => eprintln!("error: {message}"),
                    SessionEvent::TurnFinished { outcome } => {
                        eprintln!("\x1b[2m[{outcome:?}]\x1b[0m");
                        pending_approval = None;
                        prompt_marker();
                    }
                    _ => {}
                }
                std::io::stdout().flush()?;
            }
            _ = tokio::signal::ctrl_c() => {
                if !session.interrupt() {
                    break;
                }
            }
        }
    }
    agent.shutdown().await;
    Ok(())
}

fn prompt_marker() {
    eprint!("\n> ");
}
