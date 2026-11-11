use std::io::Write;
use std::path::PathBuf;

use futures::StreamExt;
use loom_config::{Dirs, LlmConfig};
use loom_llm::{
    Auth, CompletionChunk, CompletionRequest, CredentialStore, InputContent, LlmProvider,
    LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt, Message, ModelRef, ProviderId,
    ProviderSet, ReasoningEffort, RedirectSpec, ToolDefinition, http_client,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const USAGE: &str = "usage: llm [--data-dir DIR] <status | login PROVIDER [--device] | logout PROVIDER | api-key PROVIDER [KEY] | ask [--model PROVIDER/MODEL] [--reasoning LEVEL] PROMPT>";
const DEFAULT_MODEL: &str = "openai-codex/gpt-5.6-luna";

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> AnyResult<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = match take_opt(&mut args, "--data-dir") {
        Some(dir) => PathBuf::from(dir),
        None => Dirs::from_env()?.default_data_dir(),
    };
    let store = CredentialStore::in_dir(&data_dir);
    let auth = Auth::new(store.clone(), http_client());
    eprintln!("credentials: {}", store.path().display());

    let command = if args.is_empty() {
        String::new()
    } else {
        args.remove(0)
    };
    match command.as_str() {
        "status" => {
            println!("{}", serde_json::to_string_pretty(&auth.status().await?)?);
            Ok(())
        }
        "login" => {
            let device = take_flag(&mut args, "--device");
            let provider = ProviderId::new(args.first().ok_or(USAGE)?.as_str());
            login(&auth, &provider, device).await
        }
        "logout" => {
            let provider = ProviderId::new(args.first().ok_or(USAGE)?.as_str());
            Ok(auth.logout(&provider).await?)
        }
        "api-key" => {
            let provider = ProviderId::new(args.first().ok_or(USAGE)?.as_str());
            Ok(auth.set_api_key(&provider, args.get(1).cloned()).await?)
        }
        "ask" => {
            let model = take_opt(&mut args, "--model").unwrap_or_else(|| DEFAULT_MODEL.into());
            let model = ModelRef::parse(&model)?;
            let reasoning = match take_opt(&mut args, "--reasoning") {
                None => Some(ReasoningEffort::Low),
                Some(level) => Some(serde_json::from_value(json!(level))?),
            };
            let prompt = args.join(" ");
            if prompt.is_empty() {
                return Err(USAGE.into());
            }
            let providers = ProviderSet::from_config(&LlmConfig::default(), &auth)?;
            let provider = providers.get(&model.provider)?;
            ask(provider.as_ref(), &model.model, reasoning, &prompt).await
        }
        _ => Err(USAGE.into()),
    }
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == name)?;
    args.remove(pos);
    (pos < args.len()).then(|| args.remove(pos))
}

fn take_flag(args: &mut Vec<String>, name: &str) -> bool {
    let found = args.iter().position(|a| a == name);
    if let Some(pos) = found {
        args.remove(pos);
    }
    found.is_some()
}

async fn login(auth: &Auth, provider: &ProviderId, device: bool) -> AnyResult<()> {
    let status = auth.provider_status(provider).await?;
    let redirect = status.login_methods.iter().find_map(|m| match m {
        LoginMethodInfo::Browser { redirect } => Some(redirect.clone()),
        _ => None,
    });
    let (method, listener) = match (device, redirect) {
        (true, _) => (LoginMethod::DeviceCode, None),
        (false, Some(redirect)) => {
            let bound = TcpListener::bind(("127.0.0.1", redirect.port.unwrap_or(0))).await;
            let port = match &bound {
                Ok(listener) => listener.local_addr()?.port(),
                Err(e) => {
                    let port = redirect.port.ok_or("cannot open a loopback port")?;
                    println!("Port {port} is unavailable ({e}); paste the redirect URL instead.");
                    port
                }
            };
            let method = LoginMethod::Browser {
                redirect_uri: Some(redirect.uri(port)),
                account: None,
            };
            (method, bound.ok().map(|l| (l, redirect, port)))
        }
        (false, None) => {
            return Err(format!("{provider} has no browser login; try --device or api-key").into());
        }
    };
    let started = auth.start_login(provider, method).await?;
    let info = match started.prompt {
        LoginPrompt::DeviceCode {
            user_code,
            verification_uri,
            ..
        } => {
            println!("Open {verification_uri} and enter the code: {user_code}");
            let cancel = CancellationToken::new();
            let ctrl_c = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                ctrl_c.cancel();
            });
            auth.wait_login(&started.login_id, &cancel).await?
        }
        LoginPrompt::OpenUrl { url, .. } => {
            println!("Open this URL in a browser:\n\n{url}\n");
            println!("Waiting for the redirect, or paste the final redirect URL here:");
            let pasted = async {
                let mut line = String::new();
                BufReader::new(tokio::io::stdin())
                    .read_line(&mut line)
                    .await?;
                Ok::<_, std::io::Error>(LoginInput::Redirect {
                    url: line.trim().to_owned(),
                })
            };
            let input = tokio::select! {
                input = wait_redirect(listener) => input?,
                input = pasted => input?,
            };
            auth.complete_login(&started.login_id, input).await?
        }
    };
    println!(
        "Signed in to {}: {}",
        info.provider,
        info.label.or(info.account_id).unwrap_or_default()
    );
    Ok(())
}

async fn wait_redirect(
    listener: Option<(TcpListener, RedirectSpec, u16)>,
) -> std::io::Result<LoginInput> {
    let Some((listener, redirect, port)) = listener else {
        return std::future::pending().await;
    };
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or_default();
        if !target.starts_with(&redirect.path) {
            socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await?;
            continue;
        }
        let body = "Loom: login received. You can close this window.";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await?;
        return Ok(LoginInput::Redirect {
            url: format!("http://{}:{port}{target}", redirect.host),
        });
    }
}

async fn ask(
    provider: &dyn LlmProvider,
    model: &str,
    reasoning: Option<ReasoningEffort>,
    prompt: &str,
) -> AnyResult<()> {
    let tools = vec![ToolDefinition {
        name: "current_time".into(),
        description: "Returns the current local date and time.".into(),
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
    }];
    let mut messages = vec![Message::user_text(prompt)];
    let session_id = uuid::Uuid::new_v4().to_string();
    for _ in 0..4 {
        let request = CompletionRequest {
            model: model.to_owned(),
            system: Some("You are a concise assistant. Use tools when they help.".into()),
            messages: messages.clone(),
            tools: tools.clone(),
            reasoning,
            max_output_tokens: None,
            session_id: Some(session_id.clone()),
            ..Default::default()
        };
        let mut stream = provider.stream(request).await?;
        let mut done = None;
        while let Some(chunk) = stream.next().await {
            match chunk? {
                CompletionChunk::ReasoningDelta { delta, .. } => eprint!("\x1b[2m{delta}\x1b[0m"),
                CompletionChunk::ReasoningEnd { .. } => eprintln!(),
                CompletionChunk::TextDelta { delta, .. } => print!("{delta}"),
                CompletionChunk::TextEnd { .. } => println!(),
                CompletionChunk::ToolCallEnd { call, .. } => {
                    eprintln!("[tool call] {}({})", call.name, call.arguments)
                }
                CompletionChunk::Done(message) => done = Some(message),
                _ => {}
            }
            std::io::stdout().flush()?;
        }
        let message = done.ok_or("stream ended without a final message")?;
        eprintln!("[{:?}] usage: {:?}", message.stop_reason, message.usage);
        let calls: Vec<_> = message.tool_calls().cloned().collect();
        messages.push(Message::Assistant(message));
        if calls.is_empty() {
            return Ok(());
        }
        for call in calls {
            let output = match call.name.as_str() {
                "current_time" => format!("{:?}", std::time::SystemTime::now()),
                other => format!("unknown tool {other}"),
            };
            eprintln!("[tool result] {output}");
            messages.push(Message::ToolResult {
                call_id: call.call_id,
                tool_name: call.name,
                content: vec![InputContent::text(output)],
                is_error: false,
            });
        }
    }
    Err("too many tool rounds".into())
}
