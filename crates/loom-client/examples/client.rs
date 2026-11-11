use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

use loom_client::proto::{
    Decision, LoginMethod, LoginMethodInfo, LoginPrompt, SessionEvent, SessionId, TurnOutcome,
};
use loom_client::{
    AgentTarget, Client, Notification, RelayHost, SshTarget, connect_local, connect_ssh,
    load_client_identity, local_registry, pair_with_uri,
};
use loom_config::{AgentConfig, Dirs};
use tokio::io::{AsyncBufReadExt, BufReader};

const USAGE: &str = "usage: client [--ssh DEST [--remote-bin BIN]] [--agent-bin BIN] (--workspace PATH [--spawn] | --instance ID) [--model PROVIDER/MODEL] [--session ID]\n       client --pair URI [--label NAME] [--model ...]\n       client --relay URL --host-id ID --host-key KEY [--instance ID] [--model ...]";
const HELP: &str = "commands: /sessions /providers /models PROVIDER /model REF /login PROVIDER /key PROVIDER KEY /interrupt /quit";

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == name)?;
    args.remove(pos);
    (pos < args.len()).then(|| args.remove(pos))
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    let pos = args.iter().position(|a| a == name);
    if let Some(pos) = pos {
        args.remove(pos);
    }
    pos.is_some()
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let ssh = take_opt(&mut args, "--ssh");
    let remote_bin = take_opt(&mut args, "--remote-bin");
    let agent_bin = take_opt(&mut args, "--agent-bin").unwrap_or_else(|| "loom-agent".into());
    let workspace = take_opt(&mut args, "--workspace");
    let instance = take_opt(&mut args, "--instance");
    let spawn = take_flag(&mut args, "--spawn");
    let model = take_opt(&mut args, "--model");
    let session_id = take_opt(&mut args, "--session");
    let pair = take_opt(&mut args, "--pair");
    let label = take_opt(&mut args, "--label").unwrap_or_else(|| "example".into());
    let relay = take_opt(&mut args, "--relay");
    let host_id = take_opt(&mut args, "--host-id");
    let host_key = take_opt(&mut args, "--host-key");
    if !args.is_empty() {
        return Err(USAGE.into());
    }
    let relay_stream = if let Some(uri) = pair {
        let dirs = Dirs::from_env()?;
        let identity = load_client_identity(&dirs)?;
        let (host, instance) = pair_with_uri(&uri, &identity, &label, None).await?;
        println!(
            "paired; reconnect with --relay {} --host-id {} --host-key {}",
            host.url,
            host.host,
            host.host_key_text()
        );
        Some(host.connect(Some(&instance), &identity).await?)
    } else if let Some(url) = relay {
        let (Some(host_id), Some(host_key)) = (host_id, host_key) else {
            return Err(USAGE.into());
        };
        let dirs = Dirs::from_env()?;
        let identity = load_client_identity(&dirs)?;
        let host = RelayHost::from_parts(&url, &host_id, &host_key, None)?;
        let instance = instance.clone().map(loom_client::proto::InstanceId);
        Some(host.connect(instance.as_ref(), &identity).await?)
    } else {
        None
    };
    let target = match (workspace, instance) {
        _ if relay_stream.is_some() => AgentTarget::Instance(String::new()),
        (Some(path), None) => AgentTarget::Workspace {
            path: PathBuf::from(path),
            spawn,
        },
        (None, Some(id)) => AgentTarget::Instance(id),
        _ => return Err(USAGE.into()),
    };

    let stream = match (relay_stream, ssh) {
        (Some(stream), _) => stream,
        (None, Some(destination)) => {
            let mut ssh = SshTarget::new(destination);
            if let Some(bin) = remote_bin {
                ssh.remote_bin = bin;
            }
            let dirs = Dirs::from_env()?;
            let control = dirs.default_runtime_dir().join("ssh");
            std::fs::create_dir_all(&control)?;
            ssh.control_dir = Some(control);
            connect_ssh(&ssh, &target)?
        }
        (None, None) => {
            let dirs = Dirs::from_env()?;
            let config_path = dirs.agent_config_path(None, std::env::var_os("LOOM_CONFIG"));
            let config = AgentConfig::load(&config_path, &dirs)?;
            let registry = local_registry(&dirs, &config);
            connect_local(&registry, &target, &PathBuf::from(agent_bin)).await?
        }
    };
    let (client, mut notes) = Client::connect(stream, "loom-chat-example").await?;
    let agent = client.agent().clone();
    println!(
        "connected to {} (instance {}, default model {})",
        agent.workspace.display(),
        agent.instance,
        agent.default_model.as_deref().unwrap_or("none")
    );

    let session: SessionId = match session_id {
        Some(id) => SessionId(id),
        None => client.create_session(model.as_deref()).await?.id,
    };
    let info = client.subscribe(&session, None).await?;
    println!(
        "session {} (model {})\n{HELP}",
        info.id,
        info.model.as_deref().unwrap_or("default")
    );

    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = stdin.next_line().await {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut approvals: VecDeque<String> = VecDeque::new();
    let mut streamed = false;
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                if let Some(call_id) = approvals.pop_front() {
                    let decision = match line.trim() {
                        "y" | "yes" => Decision::Allow,
                        "a" | "always" => Decision::AllowForSession,
                        _ => Decision::Deny,
                    };
                    report(client.resolve_approval(&session, &call_id, decision).await);
                    prompt_next(&approvals);
                    continue;
                }
                if !handle_line(&client, &session, line.trim()).await {
                    break;
                }
            }
            note = notes.recv() => {
                let Some(note) = note else {
                    println!("\n[connection closed]");
                    break;
                };
                match render(note, &mut streamed) {
                    Some(Approval::Required(call_id)) => {
                        approvals.push_back(call_id);
                        prompt_next(&approvals);
                    }
                    Some(Approval::Resolved(call_id)) => {
                        let was_first = approvals.front() == Some(&call_id);
                        approvals.retain(|id| *id != call_id);
                        if was_first {
                            println!();
                            prompt_next(&approvals);
                        }
                    }
                    None => {}
                }
            }
        }
    }
    Ok(())
}

fn report<T>(result: loom_client::Result<T>) {
    if let Err(err) = result {
        println!("[error] {err}");
    }
}

fn prompt_next(approvals: &VecDeque<String>) {
    if let Some(call_id) = approvals.front() {
        print!("approve {call_id}? [y]es / [a]lways / [n]o: ");
        let _ = std::io::stdout().flush();
    }
}

async fn handle_line(client: &Client, session: &SessionId, line: &str) -> bool {
    let mut words = line.split_whitespace();
    match words.next() {
        None => {}
        Some("/quit") => return false,
        Some("/interrupt") => report(client.interrupt(session).await),
        Some("/sessions") => match client.list_sessions().await {
            Ok(list) => list.iter().for_each(|s| println!("{s:?}")),
            Err(err) => println!("[error] {err}"),
        },
        Some("/providers") => match client.list_providers().await {
            Ok(list) => {
                for p in list {
                    println!(
                        "{} ({}) enabled={} signed_in={} methods={:?}",
                        p.provider,
                        p.display_name,
                        p.enabled,
                        p.signed_in(),
                        p.login_methods
                    );
                }
            }
            Err(err) => println!("[error] {err}"),
        },
        Some("/models") => match client.list_models(words.next().unwrap_or_default()).await {
            Ok(list) => list
                .iter()
                .for_each(|m| println!("{} ({})", m.model_ref(), m.display_name)),
            Err(err) => println!("[error] {err}"),
        },
        Some("/model") => match client.set_model(session, words.next()).await {
            Ok(info) => println!("model: {}", info.model.as_deref().unwrap_or("default")),
            Err(err) => println!("[error] {err}"),
        },
        Some("/key") => {
            let provider = words.next().unwrap_or_default();
            report(
                client
                    .set_api_key(provider, words.next().map(str::to_owned))
                    .await,
            );
        }
        Some("/login") => login(client, words.next().unwrap_or_default()).await,
        Some(cmd) if cmd.starts_with('/') => println!("{HELP}"),
        Some(_) => report(client.send_text(session, line).await),
    }
    true
}

async fn login(client: &Client, provider: &str) {
    let providers = match client.list_providers().await {
        Ok(list) => list,
        Err(err) => return println!("[error] {err}"),
    };
    let Some(status) = providers.iter().find(|p| p.provider == provider) else {
        return println!("[error] unknown provider {provider}");
    };
    if !status.login_methods.contains(&LoginMethodInfo::DeviceCode) {
        return println!(
            "{provider} supports {:?}; this example only does device code login (or /key)",
            status.login_methods
        );
    }
    match client.start_login(provider, LoginMethod::DeviceCode).await {
        Ok(started) => {
            if let LoginPrompt::DeviceCode {
                user_code,
                verification_uri,
                ..
            } = started.prompt
            {
                println!("open {verification_uri} and enter {user_code}; waiting...");
            }
        }
        Err(err) => println!("[error] {err}"),
    }
}

enum Approval {
    Required(String),
    Resolved(String),
}

fn render(note: Notification, streamed: &mut bool) -> Option<Approval> {
    let mut out = std::io::stdout().lock();
    match note {
        Notification::Event { event, .. } => match event.event {
            SessionEvent::UserMessage { .. } | SessionEvent::StateChanged { .. } => {}
            SessionEvent::AssistantDelta { text } => {
                *streamed = true;
                let _ = write!(out, "{text}");
            }
            SessionEvent::ReasoningDelta { text } => {
                let _ = write!(out, "\x1b[2m{text}\x1b[0m");
            }
            SessionEvent::AssistantMessage { text, .. } => {
                if *streamed {
                    let _ = writeln!(out);
                } else if !text.is_empty() {
                    let _ = writeln!(out, "{text}");
                }
                *streamed = false;
            }
            SessionEvent::ToolCallRequested { call } => {
                let _ = writeln!(out, "\x1b[36m[{}] {}\x1b[0m", call.name, call.arguments);
            }
            SessionEvent::ApprovalRequired { call_id, reason } => {
                let _ = writeln!(out, "[approval needed] {reason}");
                return Some(Approval::Required(call_id));
            }
            SessionEvent::ApprovalResolved { call_id, decision } => {
                let _ = writeln!(out, "\x1b[2m[{decision:?}]\x1b[0m");
                return Some(Approval::Resolved(call_id));
            }
            SessionEvent::ToolCallStarted { .. } => {}
            SessionEvent::ToolOutput { chunk, .. } => {
                let _ = out.write_all(&chunk.data);
            }
            SessionEvent::ToolCallFinished { result, .. } => {
                let first = result.content.lines().next().unwrap_or_default();
                let tag = if result.is_error { "error" } else { "done" };
                let _ = writeln!(out, "\x1b[36m[{tag}] {first}\x1b[0m");
            }
            SessionEvent::Usage { usage } => {
                let _ = writeln!(
                    out,
                    "\x1b[2m[tokens in {} out {}]\x1b[0m",
                    usage.input, usage.output
                );
            }
            SessionEvent::TurnFinished { outcome } => {
                let tag = match outcome {
                    TurnOutcome::Completed => "completed",
                    TurnOutcome::Interrupted => "interrupted",
                    TurnOutcome::Failed => "failed",
                };
                let _ = writeln!(out, "\x1b[2m[{tag}]\x1b[0m");
            }
            SessionEvent::Warning { message } => {
                let _ = writeln!(out, "[warning] {message}");
            }
            SessionEvent::Error { message } => {
                let _ = writeln!(out, "[error] {message}");
            }
            SessionEvent::ContextCompacted {
                removed_messages,
                summarized,
            } => {
                let how = if summarized { "summarized" } else { "trimmed" };
                let _ = writeln!(out, "[context {how}: {removed_messages} messages]");
            }
            SessionEvent::ModelChanged { model, reasoning } => {
                let effort = reasoning.map_or("default".into(), |e| format!("{e:?}"));
                let _ = writeln!(out, "[model: {model}, reasoning effort: {effort}]");
            }
        },
        Notification::LoginFinished { result, .. } => match result {
            Ok(account) => {
                let _ = writeln!(out, "[signed in to {}]", account.provider);
            }
            Err(err) => {
                let _ = writeln!(out, "[login failed] {err}");
            }
        },
        Notification::Notice(notice) => {
            let _ = writeln!(out, "[notice] {notice:?}");
        }
        Notification::TerminalOutput { .. } | Notification::TerminalExited { .. } => {}
    }
    let _ = out.flush();
    None
}
