use std::io::Write;
use std::path::PathBuf;

use futures::StreamExt;
use loom_config::{CodexProviderConfig, Dirs};
use loom_llm::auth::CallbackParams;
use loom_llm::auth::codex::CALLBACK_PORT;
use loom_llm::{
    Auth, CodexProvider, CompletionChunk, CompletionRequest, CredentialStore, InputContent,
    LlmProvider, LoginInput, LoginMethod, LoginStarted, Message, ProviderId, ReasoningEffort,
    ToolDefinition, http_client,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

const USAGE: &str = "usage: codex [--data-dir DIR] <login [--device] | status | logout | ask [--model M] [--reasoning LEVEL] PROMPT>";

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> AnyResult<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let data_dir = match take_opt(&mut args, "--data-dir") {
        Some(dir) => PathBuf::from(dir),
        None => Dirs::from_env()?.default_data_dir(),
    };
    let store = CredentialStore::in_dir(&data_dir);
    let http = http_client();
    let auth = Auth::new(store.clone(), http.clone());
    eprintln!("credentials: {}", store.path().display());

    match args.first().map(String::as_str) {
        Some("login") => {
            if args.iter().any(|a| a == "--device") {
                login_device(&auth).await
            } else {
                login_browser(&auth).await
            }
        }
        Some("status") => {
            println!("{}", serde_json::to_string_pretty(&auth.status().await?)?);
            Ok(())
        }
        Some("logout") => Ok(auth.logout(ProviderId::OpenAiCodex).await?),
        Some("ask") => {
            args.remove(0);
            let model = take_opt(&mut args, "--model").unwrap_or_else(|| "gpt-5.6-luna".into());
            let reasoning = match take_opt(&mut args, "--reasoning").as_deref() {
                None => Some(ReasoningEffort::Low),
                Some("none") => Some(ReasoningEffort::None),
                Some("minimal") => Some(ReasoningEffort::Minimal),
                Some("low") => Some(ReasoningEffort::Low),
                Some("medium") => Some(ReasoningEffort::Medium),
                Some("high") => Some(ReasoningEffort::High),
                Some("xhigh") => Some(ReasoningEffort::Xhigh),
                Some(other) => return Err(format!("unknown reasoning level {other}").into()),
            };
            let prompt = args.join(" ");
            if prompt.is_empty() {
                return Err(USAGE.into());
            }
            let provider = CodexProvider::new(http, auth, &CodexProviderConfig::default());
            ask(&provider, &model, reasoning, &prompt).await
        }
        _ => Err(USAGE.into()),
    }
}

fn take_opt(args: &mut Vec<String>, name: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == name)?;
    args.remove(pos);
    (pos < args.len()).then(|| args.remove(pos))
}

async fn login_device(auth: &Auth) -> AnyResult<()> {
    let started = auth
        .start_login(ProviderId::OpenAiCodex, LoginMethod::DeviceCode)
        .await?;
    let LoginStarted::DeviceCode {
        login_id,
        user_code,
        verification_uri,
        ..
    } = started
    else {
        unreachable!()
    };
    println!("Open {verification_uri} and enter the code: {user_code}");
    let cancel = CancellationToken::new();
    let ctrl_c = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        ctrl_c.cancel();
    });
    let info = auth.wait_device_login(&login_id, &cancel).await?;
    println!("Signed in, account {}", info.account_id.unwrap_or_default());
    Ok(())
}

async fn login_browser(auth: &Auth) -> AnyResult<()> {
    let started = auth
        .start_login(
            ProviderId::OpenAiCodex,
            LoginMethod::Browser {
                redirect_uri: None,
                account: None,
            },
        )
        .await?;
    let LoginStarted::Browser { login_id, url, .. } = started else {
        unreachable!()
    };
    println!("Open this URL in a browser:\n\n{url}\n");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await;
    match &listener {
        Ok(_) => println!(
            "Waiting for the callback on port {CALLBACK_PORT}, or paste the final redirect URL here:"
        ),
        Err(e) => println!(
            "Port {CALLBACK_PORT} is unavailable ({e}). After signing in, paste the final redirect URL here:"
        ),
    }
    let pasted = async {
        let mut line = String::new();
        BufReader::new(tokio::io::stdin())
            .read_line(&mut line)
            .await?;
        Ok::<_, std::io::Error>(LoginInput::PastedRedirect { url: line })
    };
    let callback = async {
        let Ok(listener) = listener else {
            return std::future::pending().await;
        };
        loop {
            let (mut socket, _) = listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await?;
            let request = String::from_utf8_lossy(&buf[..n]);
            let target = request.split_whitespace().nth(1).unwrap_or_default();
            let Some(query) = target.strip_prefix("/auth/callback") else {
                socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                continue;
            };
            let body = "Loom: login received. You can close this window.";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await?;
            return Ok::<_, std::io::Error>(LoginInput::Callback(CallbackParams::from_query(
                query,
            )));
        }
    };
    let input = tokio::select! {
        input = callback => input?,
        input = pasted => input?,
    };
    let info = auth.complete_login(&login_id, input).await?;
    println!("Signed in, account {}", info.account_id.unwrap_or_default());
    Ok(())
}

async fn ask(
    provider: &CodexProvider,
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
