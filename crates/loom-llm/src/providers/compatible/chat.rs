use std::collections::{BTreeMap, VecDeque};

use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::error::{LlmError, Result};
use crate::provider::{ChunkStream, ProviderId};
use crate::sse::{SseEvent, decode_sse};
use crate::types::{
    AssistantContent, AssistantMessage, CompletionChunk, CompletionRequest, InputContent, Message,
    ReasoningEffort, StopReason, ToolCall, Usage,
};

fn text_of(content: &[InputContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            InputContent::Text { text } => Some(text.as_str()),
            InputContent::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user_content(content: &[InputContent]) -> Value {
    if content
        .iter()
        .all(|c| matches!(c, InputContent::Text { .. }))
    {
        return Value::String(text_of(content));
    }
    Value::Array(
        content
            .iter()
            .map(|c| match c {
                InputContent::Text { text } => json!({"type": "text", "text": text}),
                InputContent::Image { mime_type, data } => json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{mime_type};base64,{data}")}
                }),
            })
            .collect(),
    )
}

fn arguments_text(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

fn effort(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High | ReasoningEffort::Xhigh => Some("high"),
    }
}

pub(crate) fn build_body(request: &CompletionRequest) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        match message {
            Message::User { content } => {
                messages.push(json!({"role": "user", "content": user_content(content)}));
            }
            Message::Assistant(assistant) => {
                let text = assistant.text();
                let calls: Vec<Value> = assistant
                    .tool_calls()
                    .map(|call| {
                        json!({
                            "id": call.call_id,
                            "type": "function",
                            "function": {"name": call.name, "arguments": arguments_text(&call.arguments)}
                        })
                    })
                    .collect();
                let mut entry = json!({
                    "role": "assistant",
                    "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                });
                if !calls.is_empty() {
                    entry["tool_calls"] = Value::Array(calls);
                }
                messages.push(entry);
            }
            Message::ToolResult {
                call_id, content, ..
            } => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": text_of(content),
                }));
            }
        }
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect(),
        );
    }
    if let Some(effort) = request.reasoning.and_then(effort) {
        body["reasoning_effort"] = json!(effort);
    }
    if let Some(max) = request.max_output_tokens {
        body["max_tokens"] = json!(max);
    }
    body
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

pub(crate) struct ChatState {
    provider: ProviderId,
    model: String,
    started: bool,
    text: Option<String>,
    reasoning: Option<String>,
    calls: BTreeMap<u64, PendingCall>,
    finish: Option<String>,
    usage: Usage,
    response_id: Option<String>,
}

impl ChatState {
    pub(crate) fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: ProviderId::new(provider),
            model: model.to_owned(),
            started: false,
            text: None,
            reasoning: None,
            calls: BTreeMap::new(),
            finish: None,
            usage: Usage::default(),
            response_id: None,
        }
    }

    pub(crate) fn handle(&mut self, event: &Value) -> Result<Vec<CompletionChunk>> {
        let mut out = Vec::new();
        if let Some(error) = event.get("error") {
            return Err(LlmError::Stream {
                provider: self.provider.to_string(),
                code: error.get("code").and_then(|c| {
                    c.as_str()
                        .map(str::to_owned)
                        .or_else(|| Some(c.to_string()))
                }),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
                    .to_owned(),
            });
        }
        if !self.started {
            self.started = true;
            self.response_id = event.get("id").and_then(Value::as_str).map(str::to_owned);
            out.push(CompletionChunk::Start {
                response_id: self.response_id.clone(),
            });
        }
        if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
            let get = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
            self.usage = Usage {
                input: get(usage.get("prompt_tokens")),
                output: get(usage.get("completion_tokens")),
                cache_read: get(usage.pointer("/prompt_tokens_details/cached_tokens")),
                cache_write: 0,
                reasoning: get(usage.pointer("/completion_tokens_details/reasoning_tokens")),
                total: get(usage.get("total_tokens")),
            };
        }
        let Some(choice) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return Ok(out);
        };
        if let Some(delta) = choice.get("delta") {
            let reasoning = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            if let Some(reasoning) = reasoning {
                if self.reasoning.is_none() {
                    self.reasoning = Some(String::new());
                    out.push(CompletionChunk::ReasoningStart { index: 0 });
                }
                if let Some(buffer) = &mut self.reasoning {
                    buffer.push_str(reasoning);
                }
                out.push(CompletionChunk::ReasoningDelta {
                    index: 0,
                    delta: reasoning.to_owned(),
                });
            }
            if let Some(text) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                if self.text.is_none() {
                    self.text = Some(String::new());
                    out.push(CompletionChunk::TextStart { index: 1 });
                }
                if let Some(buffer) = &mut self.text {
                    buffer.push_str(text);
                }
                out.push(CompletionChunk::TextDelta {
                    index: 1,
                    delta: text.to_owned(),
                });
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                let pending = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    pending.id = id.to_owned();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    pending.name.push_str(name);
                }
                let chunk_index = 2 + index as usize;
                if !pending.started && !pending.name.is_empty() {
                    pending.started = true;
                    out.push(CompletionChunk::ToolCallStart {
                        index: chunk_index,
                        call_id: pending.id.clone(),
                        name: pending.name.clone(),
                    });
                }
                if let Some(args) = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .filter(|a| !a.is_empty())
                {
                    pending.arguments.push_str(args);
                    out.push(CompletionChunk::ToolCallDelta {
                        index: chunk_index,
                        delta: args.to_owned(),
                    });
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_owned());
        }
        Ok(out)
    }

    pub(crate) fn finish(self) -> Result<(Vec<CompletionChunk>, AssistantMessage)> {
        if !self.started {
            return Err(LlmError::protocol(
                self.provider.as_str(),
                "the stream ended before any response",
            ));
        }
        let mut chunks = Vec::new();
        let mut content = Vec::new();
        if let Some(summary) = self.reasoning {
            chunks.push(CompletionChunk::ReasoningEnd {
                index: 0,
                summary: summary.clone(),
            });
            content.push(AssistantContent::Reasoning {
                summary,
                provider_data: None,
            });
        }
        if let Some(text) = self.text {
            chunks.push(CompletionChunk::TextEnd {
                index: 1,
                text: text.clone(),
            });
            content.push(AssistantContent::Text {
                text,
                provider_data: None,
            });
        }
        let has_calls = !self.calls.is_empty();
        for (index, call) in self.calls {
            let arguments = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments).unwrap_or(Value::String(call.arguments))
            };
            let id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id
            };
            let call = ToolCall {
                call_id: id,
                name: call.name,
                arguments,
                provider_data: None,
            };
            chunks.push(CompletionChunk::ToolCallEnd {
                index: 2 + index as usize,
                call: call.clone(),
            });
            content.push(AssistantContent::ToolCall(call));
        }
        let stop_reason = match self.finish.as_deref() {
            _ if has_calls => StopReason::ToolUse,
            Some("length") => StopReason::Length,
            _ => StopReason::Stop,
        };
        Ok((
            chunks,
            AssistantMessage {
                provider: self.provider,
                model: self.model,
                content,
                usage: self.usage,
                stop_reason,
                response_id: self.response_id,
            },
        ))
    }
}

struct StreamState<S> {
    events: S,
    state: Option<ChatState>,
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

fn events_to_chunks<S>(
    provider: &str,
    model: &str,
    events: S,
) -> impl Stream<Item = Result<CompletionChunk>> + Send + 'static
where
    S: Stream<Item = reqwest::Result<SseEvent>> + Unpin + Send + 'static,
{
    let initial = StreamState {
        events,
        state: Some(ChatState::new(provider, model)),
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
            let finished = match st.events.next().await {
                Some(Ok(event)) => {
                    let data = event.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        true
                    } else {
                        let value: Value = match serde_json::from_str(data) {
                            Ok(value) => value,
                            Err(err) => {
                                st.done = true;
                                let err =
                                    LlmError::protocol(&st.provider, format!("bad event: {err}"));
                                return Some((Err(err), st));
                            }
                        };
                        let state = st.state.as_mut()?;
                        match state.handle(&value) {
                            Ok(chunks) => st.queue.extend(chunks),
                            Err(err) => {
                                st.done = true;
                                return Some((Err(err), st));
                            }
                        }
                        false
                    }
                }
                Some(Err(err)) => {
                    st.done = true;
                    return Some((Err(LlmError::Http(err)), st));
                }
                None => true,
            };
            if finished {
                st.done = true;
                let state = st.state.take()?;
                match state.finish() {
                    Ok((chunks, message)) => {
                        st.queue.extend(chunks);
                        st.queue.push_back(CompletionChunk::Done(message));
                    }
                    Err(err) => return Some((Err(err), st)),
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDefinition;

    #[test]
    fn builds_chat_messages_with_tools() {
        let request = CompletionRequest {
            model: "m".into(),
            system: Some("sys".into()),
            messages: vec![
                Message::user_text("hi"),
                Message::Assistant(AssistantMessage {
                    provider: ProviderId::new("p"),
                    model: "m".into(),
                    content: vec![AssistantContent::ToolCall(ToolCall {
                        call_id: "c1".into(),
                        name: "read".into(),
                        arguments: json!({"path": "a"}),
                        provider_data: None,
                    })],
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    response_id: None,
                }),
                Message::ToolResult {
                    call_id: "c1".into(),
                    tool_name: "read".into(),
                    content: vec![InputContent::text("data")],
                    is_error: false,
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({"type": "object"}),
            }],
            reasoning: Some(ReasoningEffort::Xhigh),
            max_output_tokens: None,
            session_id: None,
        };
        let body = build_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "hi");
        assert_eq!(messages[2]["content"], Value::Null);
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a"}"#
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn parses_streamed_text_reasoning_and_tool_calls() {
        let mut state = ChatState::new("p", "m");
        let events = [
            json!({"id": "r1", "choices": [{"delta": {"role": "assistant", "reasoning_content": "think"}}]}),
            json!({"choices": [{"delta": {"content": "Hel"}}]}),
            json!({"choices": [{"delta": {"content": "lo"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "read", "arguments": "{\"pa"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"x\"}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
        ];
        let mut chunks = Vec::new();
        for event in &events {
            chunks.extend(state.handle(event).unwrap());
        }
        let (end, message) = state.finish().unwrap();
        chunks.extend(end);
        assert!(matches!(chunks[0], CompletionChunk::Start { .. }));
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                CompletionChunk::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert_eq!(message.text(), "Hello");
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage.input, 10);
        let call = message.tool_calls().next().unwrap();
        assert_eq!(call.call_id, "call_a");
        assert_eq!(call.arguments, json!({"path": "x"}));
        assert!(
            matches!(&message.content[0], AssistantContent::Reasoning { summary, .. } if summary == "think")
        );
    }

    #[test]
    fn reports_stream_errors() {
        let mut state = ChatState::new("p", "m");
        let err = state
            .handle(&json!({"error": {"message": "overloaded", "code": "busy"}}))
            .unwrap_err();
        assert!(err.to_string().contains("overloaded"));
        assert!(ChatState::new("p", "m").finish().is_err());
    }
}
