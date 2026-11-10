use std::collections::{BTreeMap, VecDeque};

use futures::StreamExt;
use serde_json::Value;

use super::ID;
use crate::error::{LlmError, Result};
use crate::provider::ChunkStream;
use crate::sse::decode_sse;
use crate::types::{
    AssistantContent, AssistantMessage, CompletionChunk, StopReason, ToolCall, Usage,
};

struct Block {
    data: Value,
    arguments: String,
    closed: bool,
}

struct State {
    model: String,
    response_id: Option<String>,
    blocks: BTreeMap<usize, Block>,
    tools: Vec<String>,
    usage: Usage,
    stop: Option<StopReason>,
    started: bool,
    finished: bool,
}

fn protocol(message: &str) -> LlmError {
    LlmError::protocol(ID.as_str(), message)
}

fn string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| protocol("missing string in stream event"))
}

fn append_string(value: &mut Value, delta: &str) -> Result<()> {
    match value {
        Value::String(text) => {
            text.push_str(delta);
            Ok(())
        }
        _ => Err(protocol("expected string content")),
    }
}

impl State {
    fn new(model: String, tools: Vec<String>) -> Self {
        Self {
            model,
            response_id: None,
            blocks: BTreeMap::new(),
            tools,
            usage: Usage::default(),
            stop: None,
            started: false,
            finished: false,
        }
    }

    fn usage(&mut self, value: &Value) {
        for (key, dest) in [
            ("input_tokens", &mut self.usage.input),
            ("output_tokens", &mut self.usage.output),
            ("cache_read_input_tokens", &mut self.usage.cache_read),
            ("cache_creation_input_tokens", &mut self.usage.cache_write),
        ] {
            if let Some(n) = value[key].as_u64() {
                *dest = n;
            }
        }
        self.usage.total =
            self.usage.input + self.usage.output + self.usage.cache_read + self.usage.cache_write;
    }

    fn handle(&mut self, event: Value) -> Result<Vec<CompletionChunk>> {
        let mut chunks = Vec::new();
        match event["type"].as_str() {
            Some("ping") => {}
            Some("error") => {
                return Err(LlmError::Stream {
                    provider: ID.to_string(),
                    code: event["error"]["type"].as_str().map(str::to_owned),
                    message: event["error"]["message"]
                        .as_str()
                        .unwrap_or("Anthropic stream failed")
                        .into(),
                });
            }
            Some("message_start") => {
                if self.started {
                    return Err(protocol("duplicate message_start"));
                }
                self.started = true;
                self.response_id = event["message"]["id"].as_str().map(str::to_owned);
                self.usage(&event["message"]["usage"]);
                chunks.push(CompletionChunk::Start {
                    response_id: self.response_id.clone(),
                });
            }
            Some("content_block_start" | "content_block_delta" | "content_block_stop") => {
                if !self.started {
                    return Err(protocol("content before message_start"));
                }
                let index = event["index"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| protocol("missing content block index"))?;
                match event["type"].as_str().unwrap() {
                    "content_block_start" => {
                        if self.blocks.contains_key(&index) {
                            return Err(protocol("duplicate content block"));
                        }
                        let mut data = event["content_block"].clone();
                        match data["type"].as_str() {
                            Some("text") => {
                                let text = string(&data, "text")?;
                                chunks.push(CompletionChunk::TextStart { index });
                                if !text.is_empty() {
                                    chunks.push(CompletionChunk::TextDelta { index, delta: text });
                                }
                            }
                            Some("thinking") => {
                                let text = string(&data, "thinking")?;
                                chunks.push(CompletionChunk::ReasoningStart { index });
                                if !text.is_empty() {
                                    chunks.push(CompletionChunk::ReasoningDelta {
                                        index,
                                        delta: text,
                                    });
                                }
                            }
                            Some("redacted_thinking") => {}
                            Some("tool_use") => {
                                let name = string(&data, "name")?;
                                let name = self
                                    .tools
                                    .iter()
                                    .find(|n| n.eq_ignore_ascii_case(&name))
                                    .cloned()
                                    .unwrap_or(name);
                                data["name"] = name.clone().into();
                                chunks.push(CompletionChunk::ToolCallStart {
                                    index,
                                    call_id: string(&data, "id")?,
                                    name,
                                });
                            }
                            _ => return Err(protocol("unsupported content block type")),
                        }
                        self.blocks.insert(
                            index,
                            Block {
                                data,
                                arguments: String::new(),
                                closed: false,
                            },
                        );
                    }
                    "content_block_delta" => {
                        let block = self
                            .blocks
                            .get_mut(&index)
                            .filter(|b| !b.closed)
                            .ok_or_else(|| protocol("delta for unknown or closed block"))?;
                        let delta = &event["delta"];
                        match (block.data["type"].as_str(), delta["type"].as_str()) {
                            (Some("text"), Some("text_delta")) => {
                                let delta = string(delta, "text")?;
                                append_string(&mut block.data["text"], &delta)?;
                                chunks.push(CompletionChunk::TextDelta { index, delta });
                            }
                            (Some("thinking"), Some("thinking_delta")) => {
                                let delta = string(delta, "thinking")?;
                                append_string(&mut block.data["thinking"], &delta)?;
                                chunks.push(CompletionChunk::ReasoningDelta { index, delta });
                            }
                            (Some("thinking"), Some("signature_delta")) => {
                                let delta = string(delta, "signature")?;
                                if block.data["signature"].is_null() {
                                    block.data["signature"] = Value::String(String::new());
                                }
                                append_string(&mut block.data["signature"], &delta)?;
                            }
                            (Some("tool_use"), Some("input_json_delta")) => {
                                let delta = string(delta, "partial_json")?;
                                block.arguments.push_str(&delta);
                                chunks.push(CompletionChunk::ToolCallDelta { index, delta });
                            }
                            _ => return Err(protocol("unexpected content block delta")),
                        }
                    }
                    _ => {
                        let block = self
                            .blocks
                            .get_mut(&index)
                            .filter(|b| !b.closed)
                            .ok_or_else(|| protocol("stop for unknown or closed block"))?;
                        block.closed = true;
                        match block.data["type"].as_str() {
                            Some("text") => chunks.push(CompletionChunk::TextEnd {
                                index,
                                text: string(&block.data, "text")?,
                            }),
                            Some("thinking") => chunks.push(CompletionChunk::ReasoningEnd {
                                index,
                                summary: string(&block.data, "thinking")?,
                            }),
                            Some("tool_use") => {
                                if !block.arguments.is_empty() {
                                    block.data["input"] = serde_json::from_str(&block.arguments)
                                        .map_err(|_| protocol("invalid tool argument JSON"))?;
                                }
                                if !block.data["input"].is_object() {
                                    return Err(protocol("tool arguments must be an object"));
                                }
                                chunks.push(CompletionChunk::ToolCallEnd {
                                    index,
                                    call: tool_call(&block.data)?,
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("message_delta") => {
                if !self.started {
                    return Err(protocol("message_delta before message_start"));
                }
                self.usage(&event["usage"]);
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.stop = Some(match reason {
                        "end_turn" | "stop_sequence" | "pause_turn" | "refusal" => StopReason::Stop,
                        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                        "tool_use" => StopReason::ToolUse,
                        _ => return Err(protocol("unknown stop reason")),
                    });
                }
            }
            Some("message_stop") => {
                let stop_reason = self
                    .stop
                    .ok_or_else(|| protocol("message stopped without a stop reason"))?;
                if self.blocks.values().any(|b| !b.closed) {
                    return Err(protocol("message stopped with an open block"));
                }
                let content = self
                    .blocks
                    .values()
                    .map(|block| match block.data["type"].as_str() {
                        Some("text") => Ok(AssistantContent::text(string(&block.data, "text")?)),
                        Some("tool_use") => Ok(AssistantContent::ToolCall(tool_call(&block.data)?)),
                        Some("thinking") => Ok(AssistantContent::Reasoning {
                            summary: string(&block.data, "thinking")?,
                            provider_data: Some(block.data.clone()),
                        }),
                        Some("redacted_thinking") => Ok(AssistantContent::Reasoning {
                            summary: String::new(),
                            provider_data: Some(block.data.clone()),
                        }),
                        _ => Err(protocol("unsupported content block")),
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.finished = true;
                chunks.push(CompletionChunk::Done(AssistantMessage {
                    provider: ID,
                    model: self.model.clone(),
                    content,
                    usage: self.usage,
                    stop_reason,
                    response_id: self.response_id.clone(),
                }));
            }
            Some(_) => {}
            None => return Err(protocol("missing stream event type")),
        }
        Ok(chunks)
    }
}

fn tool_call(data: &Value) -> Result<ToolCall> {
    Ok(ToolCall {
        call_id: string(data, "id")?,
        name: string(data, "name")?,
        arguments: data["input"].clone(),
        provider_data: None,
    })
}

pub(super) fn chunk_stream(
    model: &str,
    tools: Vec<String>,
    response: reqwest::Response,
) -> ChunkStream {
    let events = decode_sse(response.bytes_stream().boxed()).boxed();
    let state = State::new(model.into(), tools);
    futures::stream::unfold(
        (events, state, VecDeque::new(), false),
        |(mut events, mut state, mut queue, mut done)| async move {
            loop {
                if let Some(chunk) = queue.pop_front() {
                    return Some((Ok(chunk), (events, state, queue, done)));
                }
                if done || state.finished {
                    return None;
                }
                let result = match events.next().await {
                    Some(Ok(event)) => serde_json::from_str(&event.data)
                        .map_err(|_| protocol("invalid SSE JSON"))
                        .and_then(|event| state.handle(event)),
                    Some(Err(error)) => Err(LlmError::Http(error)),
                    None => Err(protocol("stream ended before message_stop")),
                };
                match result {
                    Ok(chunks) => queue.extend(chunks),
                    Err(error) => {
                        done = true;
                        return Some((Err(error), (events, state, queue, done)));
                    }
                }
            }
        },
    )
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_counts_cached_prompt_tokens_in_the_context() {
        let mut state = State::new("m".into(), vec![]);
        let usage = json!({
            "input_tokens": 3,
            "cache_read_input_tokens": 50_000,
            "cache_creation_input_tokens": 1_200,
            "output_tokens": 1
        });
        state
            .handle(json!({"type": "message_start", "message": {"id": "m", "usage": usage}}))
            .unwrap();
        state.handle(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})).unwrap();
        state
            .handle(json!({"type": "content_block_stop", "index": 0}))
            .unwrap();
        state
            .handle(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 437}}))
            .unwrap();
        let chunks = state.handle(json!({"type": "message_stop"})).unwrap();
        let Some(CompletionChunk::Done(message)) = chunks.last() else {
            panic!("no message: {chunks:?}");
        };
        let usage = message.usage;
        assert_eq!((usage.input, usage.output), (3, 437));
        assert_eq!((usage.cache_read, usage.cache_write), (50_000, 1_200));
        assert_eq!(usage.prompt_tokens(), 51_203);
        assert_eq!(usage.context_tokens(), 51_640);
    }

    #[test]
    fn rejects_invalid_event_sequences() {
        let mut state = State::new("m".into(), vec![]);
        assert!(
            state
                .handle(json!({"type": "content_block_delta", "index": 0}))
                .is_err()
        );
        state
            .handle(json!({"type": "message_start", "message": {"id": "m"}}))
            .unwrap();
        assert!(state.handle(json!({"type": "message_start"})).is_err());
        assert!(state.handle(json!({"type": "message_stop"})).is_err());
        state.handle(json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})).unwrap();
        assert!(state.handle(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}})).is_err());
    }
}
