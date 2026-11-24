use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hyper::Method;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::{Exec, Incus, IncusError, Io, Operation, Result, encode};

const CHUNK: usize = 32 * 1024;
const END_WAIT: Duration = Duration::from_secs(30);

type Socket = WebSocketStream<Box<dyn Io>>;

fn protocol(message: impl Into<String>) -> IncusError {
    IncusError::Protocol(message.into())
}

async fn pump_in<R: AsyncRead + Unpin>(mut input: R, socket: &mut Socket) {
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let message = match input.read(&mut buffer).await {
            Ok(0) | Err(_) => Message::Text("".into()),
            Ok(n) => Message::Binary(buffer[..n].to_vec().into()),
        };
        let end = matches!(message, Message::Text(_));
        if socket.send(message).await.is_err() || end {
            return;
        }
    }
}

async fn pump_out<W: AsyncWrite + Unpin>(
    socket: &mut Socket,
    mut output: W,
) -> std::io::Result<()> {
    while let Some(message) = socket.next().await {
        match message {
            Ok(Message::Binary(bytes)) => {
                output.write_all(&bytes).await?;
                output.flush().await?;
            }
            Ok(Message::Text(text)) if text.is_empty() => break,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    output.flush().await
}

impl Incus {
    async fn websocket(&self, operation: &str, secret: &str) -> Result<Socket> {
        let path = format!(
            "/1.0/operations/{}/websocket?secret={}",
            encode(operation),
            encode(secret)
        );
        let url = format!("ws://{}{}", self.authority(), self.uri(&path, true));
        let stream = self.connect().await?;
        let (socket, _) = tokio_tungstenite::client_async(url, stream)
            .await
            .map_err(|err| IncusError::Http(format!("cannot open the exec stream: {err}")))?;
        Ok(socket)
    }

    pub async fn exec_stream<R, W, E>(
        &self,
        instance: &str,
        exec: &Exec,
        stdin: R,
        stdout: W,
        stderr: E,
    ) -> Result<i32>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
        E: AsyncWrite + Unpin + Send,
    {
        let mut body = json!({
            "command": exec.command,
            "environment": exec.environment,
            "interactive": false,
            "wait-for-websocket": true,
            "record-output": false,
            "user": exec.user,
            "group": exec.group,
        });
        if let Some(cwd) = &exec.cwd {
            body["cwd"] = Value::String(cwd.clone());
        }
        let path = format!("/1.0/instances/{}/exec", encode(instance));
        let envelope = self.call(Method::POST, &path, Some(&body)).await?;
        let operation: Operation = serde_json::from_value(envelope.metadata)
            .map_err(|err| protocol(format!("cannot read the operation: {err}")))?;
        let fds: BTreeMap<String, String> = operation
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("fds"))
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|err| protocol(format!("cannot read the exec streams: {err}")))?
            .unwrap_or_default();
        let secret = |fd: &str| {
            fds.get(fd)
                .cloned()
                .ok_or_else(|| protocol(format!("the exec operation has no stream {fd}")))
        };
        let mut control = self.websocket(&operation.id, &secret("control")?).await?;
        let mut input = self.websocket(&operation.id, &secret("0")?).await?;
        let mut output = self.websocket(&operation.id, &secret("1")?).await?;
        let mut errors = self.websocket(&operation.id, &secret("2")?).await?;
        let written = {
            let send = pump_in(stdin, &mut input);
            let receive = async {
                tokio::try_join!(pump_out(&mut output, stdout), pump_out(&mut errors, stderr))
            };
            tokio::pin!(send, receive);
            let mut sent = false;
            loop {
                tokio::select! {
                    result = &mut receive => break result,
                    () = &mut send, if !sent => sent = true,
                }
            }
        };
        if let Err(err) = written {
            let _ = control.close(None).await;
            return Err(protocol(format!("cannot write the output: {err}")));
        }
        let done = self.wait(&operation.id, END_WAIT).await;
        for socket in [&mut control, &mut input, &mut output, &mut errors] {
            let _ = socket.close(None).await;
        }
        let done = done?;
        done.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("return"))
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok())
            .ok_or_else(|| IncusError::Operation {
                status: done.status.clone(),
                message: done.err.clone(),
            })
    }
}
