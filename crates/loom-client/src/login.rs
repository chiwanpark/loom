use std::time::Duration;

use loom_proto::RedirectSpec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::{ClientError, Result};

const MAX_REQUEST: usize = 16 * 1024;
const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Loom</title><p>Sign-in finished. You can close this window and return to Loom.</p>";

pub struct BrowserCallback {
    listener: TcpListener,
    redirect_uri: String,
    spec: RedirectSpec,
}

impl BrowserCallback {
    pub async fn bind(spec: &RedirectSpec) -> Result<Self> {
        let listener = TcpListener::bind((spec.host.as_str(), spec.port.unwrap_or(0))).await?;
        let port = listener.local_addr()?.port();
        Ok(Self {
            listener,
            redirect_uri: spec.uri(port),
            spec: spec.clone(),
        })
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub async fn wait(self, timeout: Duration) -> Result<String> {
        tokio::time::timeout(timeout, self.accept())
            .await
            .map_err(|_| ClientError::Protocol("the sign-in timed out".into()))?
    }

    async fn accept(self) -> Result<String> {
        loop {
            let (mut socket, _) = self.listener.accept().await?;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 2048];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < MAX_REQUEST {
                let n = socket.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let Some(target) = request_target(&buf) else {
                let _ = socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
                    .await;
                continue;
            };
            let path = target.split('?').next().unwrap_or_default();
            if path != self.spec.path {
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
                    )
                    .await;
                continue;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{DONE_PAGE}",
                DONE_PAGE.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
            let base = self
                .redirect_uri
                .strip_suffix(&self.spec.path)
                .unwrap_or(&self.redirect_uri);
            return Ok(format!("{base}{target}"));
        }
    }
}

fn request_target(request: &[u8]) -> Option<String> {
    let line = request.split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end();
    let mut parts = line.split(' ');
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    target.starts_with('/').then(|| target.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_the_redirect() {
        let spec = RedirectSpec {
            host: "127.0.0.1".into(),
            port: None,
            path: "/auth/callback".into(),
        };
        let callback = BrowserCallback::bind(&spec).await.unwrap();
        let uri = callback.redirect_uri().to_owned();
        assert!(uri.starts_with("http://127.0.0.1:") && uri.ends_with("/auth/callback"));
        let addr = uri
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let waiter = tokio::spawn(callback.wait(Duration::from_secs(5)));
        let mut wrong = tokio::net::TcpStream::connect(&addr).await.unwrap();
        wrong
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut sink = Vec::new();
        wrong.read_to_end(&mut sink).await.unwrap();
        assert!(sink.starts_with(b"HTTP/1.1 404"));
        let mut socket = tokio::net::TcpStream::connect(&addr).await.unwrap();
        socket
            .write_all(b"GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut body = Vec::new();
        socket.read_to_end(&mut body).await.unwrap();
        assert!(body.starts_with(b"HTTP/1.1 200"));
        let url = waiter.await.unwrap().unwrap();
        assert_eq!(url, format!("{uri}?code=abc&state=xyz"));
    }

    #[test]
    fn parses_request_lines() {
        assert_eq!(
            request_target(b"GET /a?b=c HTTP/1.1\r\n").as_deref(),
            Some("/a?b=c")
        );
        assert_eq!(request_target(b"POST /a HTTP/1.1\r\n"), None);
        assert_eq!(request_target(b"GET http://x/ HTTP/1.1\r\n"), None);
    }
}
