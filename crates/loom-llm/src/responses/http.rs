use std::collections::VecDeque;
use std::time::Duration;

use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use serde_json::Value;

use super::stream::ResponsesState;
use crate::error::{LlmError, Result};
use crate::provider::ChunkStream;
use crate::retry::RetryPolicy;
use crate::sse::{SseEvent, decode_sse};
use crate::types::CompletionChunk;

pub(crate) async fn send_with_retry<F>(
    provider: &str,
    policy: RetryPolicy,
    mut build: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut attempt = 0;
    loop {
        let result = build().send().await;
        let error = match result {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => api_error(provider, response).await,
            Err(err) => LlmError::Http(err),
        };
        if attempt >= policy.max_retries || !is_retryable(&error) {
            return Err(error);
        }
        let delay = match &error {
            LlmError::Api {
                retry_after: Some(after),
                ..
            } => {
                if *after > policy.max_delay {
                    return Err(error);
                }
                *after
            }
            _ => policy.base_delay * 2u32.pow(attempt),
        };
        tracing::debug!(provider, attempt, ?delay, error = %error, "retrying request");
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

fn is_retryable(error: &LlmError) -> bool {
    match error {
        LlmError::Http(err) => err.is_connect() || err.is_timeout(),
        LlmError::Api {
            status,
            code,
            message,
            ..
        } => {
            let text = format!("{} {message}", code.as_deref().unwrap_or_default()).to_lowercase();
            let quota = [
                "insufficient_quota",
                "usage_limit",
                "usage_not_included",
                "billing",
                "quota exceeded",
            ]
            .iter()
            .any(|k| text.contains(k));
            !quota && matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
        }
        _ => false,
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(ms) = get("retry-after-ms").and_then(|v| v.trim().parse::<f64>().ok()) {
        return Some(Duration::from_secs_f64(ms.max(0.0) / 1000.0));
    }
    get("retry-after")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|s| Duration::from_secs_f64(s.max(0.0)))
}

pub(crate) async fn api_error(provider: &str, response: reqwest::Response) -> LlmError {
    let status = response.status();
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_str(&body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let code = error.and_then(|e| {
        e.get("code")
            .or_else(|| e.get("type"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let mut message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            if body.is_empty() {
                status
                    .canonical_reason()
                    .unwrap_or("request failed")
                    .to_owned()
            } else {
                body.clone()
            }
        });
    if let Some(resets_at) = error
        .and_then(|e| e.get("resets_at"))
        .and_then(Value::as_u64)
    {
        let now = crate::store::now_unix();
        let minutes = resets_at.saturating_sub(now).div_ceil(60);
        message = format!("{message} (resets in about {minutes} min)");
    }
    LlmError::Api {
        provider: provider.to_owned(),
        status: status.as_u16(),
        code,
        message,
        retry_after,
    }
}

struct StreamState<S> {
    events: S,
    state: Option<ResponsesState>,
    queue: VecDeque<CompletionChunk>,
    provider: String,
    done: bool,
}

pub(crate) fn chunk_stream(
    provider: &str,
    model: &str,
    response: reqwest::Response,
) -> ChunkStream {
    let events: BoxStream<'static, reqwest::Result<SseEvent>> =
        decode_sse(response.bytes_stream().boxed()).boxed();
    events_to_chunks(provider, model, events).boxed()
}

pub(crate) fn events_to_chunks<S>(
    provider: &str,
    model: &str,
    events: S,
) -> impl Stream<Item = Result<CompletionChunk>> + Send + 'static
where
    S: Stream<Item = reqwest::Result<SseEvent>> + Unpin + Send + 'static,
{
    let initial = StreamState {
        events,
        state: Some(ResponsesState::new(provider, model)),
        queue: VecDeque::new(),
        provider: provider.to_owned(),
        done: false,
    };
    futures::stream::unfold(initial, |mut st| async move {
        loop {
            if let Some(chunk) = st.queue.pop_front() {
                return Some((Ok(chunk), st));
            }
            if st.done {
                return None;
            }
            let fail = |mut st: StreamState<S>, err: LlmError| {
                st.done = true;
                st.queue.clear();
                Some((Err(err), st))
            };
            let state = st.state.as_mut()?;
            if state.is_finished() {
                let state = st
                    .state
                    .take()
                    .unwrap_or_else(|| ResponsesState::new("", ""));
                st.done = true;
                match state.finish() {
                    Ok(message) => st.queue.push_back(CompletionChunk::Done(message)),
                    Err(err) => return fail(st, err),
                }
                continue;
            }
            match st.events.next().await {
                Some(Ok(event)) => {
                    let data = event.data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(data) {
                        Ok(value) => value,
                        Err(e) => {
                            let err =
                                LlmError::protocol(&st.provider, format!("invalid SSE JSON: {e}"));
                            return fail(st, err);
                        }
                    };
                    match state.handle(&value) {
                        Ok(chunks) => st.queue.extend(chunks),
                        Err(err) => return fail(st, err),
                    }
                }
                Some(Err(err)) => return fail(st, LlmError::Http(err)),
                None => {
                    let state = st
                        .state
                        .take()
                        .unwrap_or_else(|| ResponsesState::new("", ""));
                    st.done = true;
                    match state.finish() {
                        Ok(message) => st.queue.push_back(CompletionChunk::Done(message)),
                        Err(err) => return fail(st, err),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(data: &str) -> reqwest::Result<SseEvent> {
        Ok(SseEvent {
            event: None,
            data: data.to_owned(),
        })
    }

    #[tokio::test]
    async fn converts_events_and_stops_at_terminal() {
        let events = futures::stream::iter(vec![
            sse(r#"{"type":"response.created","response":{"id":"r1"}}"#),
            sse(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            ),
            sse(r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#),
            sse(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"hi"}]}}"#,
            ),
            sse(r#"{"type":"response.completed","response":{"status":"completed"}}"#),
            sse("[DONE]"),
        ]);
        let chunks: Vec<_> = events_to_chunks("openai", "m", events).collect().await;
        let last = chunks.last().unwrap().as_ref().unwrap();
        match last {
            CompletionChunk::Done(message) => assert_eq!(message.text(), "hi"),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(chunks.len(), 5);
    }

    #[tokio::test]
    async fn invalid_json_fails_once() {
        let events = futures::stream::iter(vec![sse("{nope"), sse("{}")]);
        let chunks: Vec<_> = events_to_chunks("openai", "m", events).collect().await;
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], Err(LlmError::Protocol { .. })));
    }

    #[tokio::test]
    async fn truncated_stream_fails() {
        let events = futures::stream::iter(vec![sse(
            r#"{"type":"response.created","response":{"id":"r1"}}"#,
        )]);
        let chunks: Vec<_> = events_to_chunks("openai", "m", events).collect().await;
        assert!(chunks.last().unwrap().is_err());
    }

    #[test]
    fn quota_errors_are_not_retried() {
        let api = |status, code: &str| LlmError::Api {
            provider: "openai".into(),
            status,
            code: Some(code.into()),
            message: String::new(),
            retry_after: None,
        };
        assert!(is_retryable(&api(429, "rate_limit_exceeded")));
        assert!(is_retryable(&api(503, "overloaded")));
        assert!(!is_retryable(&api(429, "insufficient_quota")));
        assert!(!is_retryable(&api(429, "usage_limit_reached")));
        assert!(!is_retryable(&api(400, "invalid_request")));
    }
}
