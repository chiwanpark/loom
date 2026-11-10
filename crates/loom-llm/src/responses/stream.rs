use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::error::{LlmError, Result};
use crate::types::{
    AssistantContent, AssistantMessage, CompletionChunk, StopReason, ToolCall, Usage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Text,
    Reasoning,
    ToolCall,
}

#[derive(Debug)]
struct Slot {
    kind: SlotKind,
    index: usize,
    arguments: String,
}

#[derive(Debug)]
pub struct ResponsesState {
    provider: String,
    model: String,
    content: Vec<AssistantContent>,
    slots: HashMap<u64, Slot>,
    unfinished: BTreeSet<usize>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    response_id: Option<String>,
    started: bool,
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

impl ResponsesState {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            content: Vec::new(),
            slots: HashMap::new(),
            unfinished: BTreeSet::new(),
            usage: Usage::default(),
            stop_reason: None,
            response_id: None,
            started: false,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.stop_reason.is_some()
    }

    fn stream_error(&self, code: Option<String>, message: String) -> LlmError {
        LlmError::Stream {
            provider: self.provider.clone(),
            code,
            message,
        }
    }

    pub fn handle(&mut self, event: &Value) -> Result<Vec<CompletionChunk>> {
        let mut out = Vec::new();
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return Ok(out);
        };
        if !self.started {
            self.started = true;
            self.response_id = event.get("response").and_then(|r| str_field(r, "id"));
            out.push(CompletionChunk::Start {
                response_id: self.response_id.clone(),
            });
        }
        let output_index = event.get("output_index").and_then(Value::as_u64);
        let delta = || {
            event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match kind {
            "response.created" => {
                if let Some(id) = event.get("response").and_then(|r| str_field(r, "id")) {
                    self.response_id = Some(id);
                }
            }
            "response.output_item.added" => {
                if let (Some(oi), Some(item)) = (output_index, event.get("item")) {
                    self.create_slot(oi, item, &mut out);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.push_delta(output_index, SlotKind::Reasoning, delta(), &mut out);
            }
            "response.reasoning_summary_part.done" => {
                self.push_delta(output_index, SlotKind::Reasoning, "\n\n".into(), &mut out);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.push_delta(output_index, SlotKind::Text, delta(), &mut out);
            }
            "response.function_call_arguments.delta" => {
                if let Some(slot) = self.slot_mut(output_index, SlotKind::ToolCall) {
                    let delta = delta();
                    slot.arguments.push_str(&delta);
                    out.push(CompletionChunk::ToolCallDelta {
                        index: slot.index,
                        delta,
                    });
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(slot), Some(args)) = (
                    self.slot_mut(output_index, SlotKind::ToolCall),
                    event.get("arguments").and_then(Value::as_str),
                ) {
                    if let Some(rest) = args.strip_prefix(slot.arguments.as_str())
                        && !rest.is_empty()
                    {
                        out.push(CompletionChunk::ToolCallDelta {
                            index: slot.index,
                            delta: rest.to_owned(),
                        });
                    }
                    slot.arguments = args.to_owned();
                }
            }
            "response.output_item.done" => {
                if let (Some(oi), Some(item)) = (output_index, event.get("item")) {
                    self.finish_item(oi, item, &mut out);
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.finalize(event.get("response"))?;
            }
            "response.failed" => {
                let response = event.get("response");
                let error = response.and_then(|r| r.get("error"));
                let code = error.and_then(|e| str_field(e, "code"));
                let message = error
                    .and_then(|e| str_field(e, "message"))
                    .or_else(|| {
                        response
                            .and_then(|r| r.get("incomplete_details"))
                            .and_then(|d| str_field(d, "reason"))
                            .map(|r| format!("incomplete: {r}"))
                    })
                    .unwrap_or_else(|| "response failed without details".into());
                return Err(self.stream_error(code, message));
            }
            "error" => {
                let nested = event.get("error");
                let code =
                    str_field(event, "code").or_else(|| nested.and_then(|e| str_field(e, "code")));
                let message = str_field(event, "message")
                    .or_else(|| nested.and_then(|e| str_field(e, "message")))
                    .unwrap_or_else(|| event.to_string());
                return Err(self.stream_error(code, message));
            }
            _ => {}
        }
        Ok(out)
    }

    fn slot_mut(&mut self, output_index: Option<u64>, kind: SlotKind) -> Option<&mut Slot> {
        self.slots
            .get_mut(&output_index?)
            .filter(|slot| slot.kind == kind)
    }

    fn push_delta(
        &mut self,
        output_index: Option<u64>,
        kind: SlotKind,
        delta: String,
        out: &mut Vec<CompletionChunk>,
    ) {
        let Some(index) = self.slot_mut(output_index, kind).map(|s| s.index) else {
            return;
        };
        match &mut self.content[index] {
            AssistantContent::Text { text, .. } => text.push_str(&delta),
            AssistantContent::Reasoning { summary, .. } => summary.push_str(&delta),
            AssistantContent::ToolCall(_) => return,
        }
        out.push(match kind {
            SlotKind::Text => CompletionChunk::TextDelta { index, delta },
            _ => CompletionChunk::ReasoningDelta { index, delta },
        });
    }

    fn create_slot(&mut self, output_index: u64, item: &Value, out: &mut Vec<CompletionChunk>) {
        let index = self.content.len();
        let (kind, content, chunk) = match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => (
                SlotKind::Reasoning,
                AssistantContent::Reasoning {
                    summary: String::new(),
                    item: None,
                },
                CompletionChunk::ReasoningStart { index },
            ),
            Some("message") => (
                SlotKind::Text,
                AssistantContent::Text {
                    text: String::new(),
                    item_id: None,
                    phase: None,
                },
                CompletionChunk::TextStart { index },
            ),
            Some("function_call") => {
                let call_id = str_field(item, "call_id").unwrap_or_default();
                let name = str_field(item, "name").unwrap_or_default();
                self.unfinished.insert(index);
                (
                    SlotKind::ToolCall,
                    AssistantContent::ToolCall(ToolCall {
                        call_id: call_id.clone(),
                        item_id: str_field(item, "id"),
                        name: name.clone(),
                        arguments: Value::Null,
                    }),
                    CompletionChunk::ToolCallStart {
                        index,
                        call_id,
                        name,
                    },
                )
            }
            _ => return,
        };
        self.content.push(content);
        self.slots.insert(
            output_index,
            Slot {
                kind,
                index,
                arguments: str_field(item, "arguments").unwrap_or_default(),
            },
        );
        out.push(chunk);
    }

    fn finish_item(&mut self, output_index: u64, item: &Value, out: &mut Vec<CompletionChunk>) {
        if !self.slots.contains_key(&output_index) {
            self.create_slot(output_index, item, out);
        }
        let Some(slot) = self.slots.remove(&output_index) else {
            return;
        };
        let index = slot.index;
        match (
            &mut self.content[index],
            item.get("type").and_then(Value::as_str),
        ) {
            (
                AssistantContent::Reasoning {
                    summary,
                    item: stored,
                },
                Some("reasoning"),
            ) => {
                let joined = |key: &str| {
                    item.get(key)
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter_map(|p| p.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default()
                };
                let text = Some(joined("summary"))
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some(joined("content")).filter(|s| !s.is_empty()));
                if let Some(text) = text {
                    *summary = text;
                }
                *stored = Some(item.clone());
                out.push(CompletionChunk::ReasoningEnd {
                    index,
                    summary: summary.clone(),
                });
            }
            (
                AssistantContent::Text {
                    text,
                    item_id,
                    phase,
                },
                Some("message"),
            ) => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    *text = parts
                        .iter()
                        .filter_map(|p| {
                            p.get("text")
                                .or_else(|| p.get("refusal"))
                                .and_then(Value::as_str)
                        })
                        .collect();
                }
                *item_id = str_field(item, "id");
                *phase = str_field(item, "phase");
                out.push(CompletionChunk::TextEnd {
                    index,
                    text: text.clone(),
                });
            }
            (AssistantContent::ToolCall(call), Some("function_call")) => {
                let raw = str_field(item, "arguments").unwrap_or(slot.arguments);
                call.arguments = if raw.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&raw).unwrap_or(Value::String(raw))
                };
                if let Some(id) = str_field(item, "id") {
                    call.item_id = Some(id);
                }
                self.unfinished.remove(&index);
                out.push(CompletionChunk::ToolCallEnd {
                    index,
                    call: call.clone(),
                });
            }
            _ => {}
        }
    }

    fn finalize(&mut self, response: Option<&Value>) -> Result<()> {
        if let Some(id) = response.and_then(|r| str_field(r, "id")) {
            self.response_id = Some(id);
        }
        if let Some(usage) = response.and_then(|r| r.get("usage")) {
            let n = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
            let input_details = usage.get("input_tokens_details");
            let cached = n(input_details.and_then(|d| d.get("cached_tokens")));
            let cache_write = n(input_details.and_then(|d| d.get("cache_write_tokens")));
            self.usage = Usage {
                input: n(usage.get("input_tokens")).saturating_sub(cached + cache_write),
                output: n(usage.get("output_tokens")),
                cache_read: cached,
                cache_write,
                reasoning: n(usage
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))),
                total: n(usage.get("total_tokens")),
            };
        }
        let status = response.and_then(|r| str_field(r, "status"));
        let reason = response
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| str_field(d, "reason"));
        let stop = match status.as_deref() {
            Some("incomplete") if reason.as_deref() == Some("max_output_tokens") => {
                StopReason::Length
            }
            Some("incomplete") => {
                return Err(self.stream_error(
                    reason.clone(),
                    match reason {
                        Some(reason) => format!("response incomplete: {reason}"),
                        None => "response incomplete without a reason".into(),
                    },
                ));
            }
            Some("failed") | Some("cancelled") => {
                return Err(
                    self.stream_error(None, format!("response {}", status.unwrap_or_default()))
                );
            }
            _ => StopReason::Stop,
        };
        let has_tool_calls = self
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        self.stop_reason = Some(if stop == StopReason::Stop && has_tool_calls {
            StopReason::ToolUse
        } else {
            stop
        });
        Ok(())
    }

    pub fn finish(self) -> Result<AssistantMessage> {
        let Some(stop_reason) = self.stop_reason else {
            return Err(LlmError::Stream {
                provider: self.provider,
                code: None,
                message: "stream ended before a terminal response event".into(),
            });
        };
        if stop_reason == StopReason::ToolUse
            && let Some(&index) = self.unfinished.iter().next()
        {
            let name = match &self.content[index] {
                AssistantContent::ToolCall(call) => call.name.clone(),
                _ => String::new(),
            };
            return Err(LlmError::Stream {
                provider: self.provider,
                code: None,
                message: format!("stream completed with an unfinished tool call: {name}"),
            });
        }
        Ok(AssistantMessage {
            provider: self.provider,
            model: self.model,
            content: self.content,
            usage: self.usage,
            stop_reason,
            response_id: self.response_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn run(events: &[Value]) -> (Vec<CompletionChunk>, Result<AssistantMessage>) {
        let mut state = ResponsesState::new("openai", "gpt-x");
        let mut chunks = Vec::new();
        for event in events {
            match state.handle(event) {
                Ok(c) => chunks.extend(c),
                Err(e) => return (chunks, Err(e)),
            }
        }
        (chunks, state.finish())
    }

    fn completed(usage: Value) -> Value {
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "usage": usage}})
    }

    #[test]
    fn text_response() {
        let (chunks, message) = run(&[
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1"}}),
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "lo"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_1", "phase": "final_answer", "content": [{"type": "output_text", "text": "Hello"}]}}),
            completed(
                json!({"input_tokens": 100, "output_tokens": 5, "total_tokens": 105, "input_tokens_details": {"cached_tokens": 40}, "output_tokens_details": {"reasoning_tokens": 2}}),
            ),
        ]);
        let message = message.unwrap();
        assert_eq!(message.text(), "Hello");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.response_id.as_deref(), Some("resp_1"));
        assert_eq!(
            message.usage,
            Usage {
                input: 60,
                output: 5,
                cache_read: 40,
                cache_write: 0,
                reasoning: 2,
                total: 105
            }
        );
        assert_eq!(
            message.content[0],
            AssistantContent::Text {
                text: "Hello".into(),
                item_id: Some("msg_1".into()),
                phase: Some("final_answer".into())
            }
        );
        assert!(matches!(chunks[0], CompletionChunk::Start { .. }));
        assert_eq!(
            chunks[2],
            CompletionChunk::TextDelta {
                index: 0,
                delta: "Hel".into()
            }
        );
    }

    #[test]
    fn reasoning_and_tool_call() {
        let reasoning_item = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Plan"}], "encrypted_content": "enc"});
        let (chunks, message) = run(&[
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Pl"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"path\":"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 1, "arguments": "{\"path\":\"a\"}"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"}}),
            completed(json!({})),
        ]);
        let message = message.unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(
            message.content[0],
            AssistantContent::Reasoning {
                summary: "Plan".into(),
                item: Some(reasoning_item)
            }
        );
        let call = message.tool_calls().next().unwrap();
        assert_eq!(call.call_id, "call_1");
        assert_eq!(call.item_id.as_deref(), Some("fc_1"));
        assert_eq!(call.arguments, json!({"path": "a"}));
        assert!(chunks.contains(&CompletionChunk::ToolCallDelta {
            index: 1,
            delta: "\"a\"}".into()
        }));
    }

    #[test]
    fn item_done_without_added_creates_slot() {
        let (_, message) = run(&[
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "id": "fc_2", "call_id": "c2", "name": "shell", "arguments": ""}}),
            json!({"type": "response.done", "response": {"status": "completed"}}),
        ]);
        let message = message.unwrap();
        assert_eq!(message.tool_calls().next().unwrap().arguments, json!({}));
        assert_eq!(message.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn unfinished_tool_call_is_an_error() {
        let (_, message) = run(&[
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "shell"}}),
            completed(json!({})),
        ]);
        assert!(
            message
                .unwrap_err()
                .to_string()
                .contains("unfinished tool call")
        );
    }

    #[test]
    fn missing_terminal_event_is_an_error() {
        let (_, message) = run(&[json!({"type": "response.created", "response": {"id": "r"}})]);
        assert!(message.unwrap_err().to_string().contains("terminal"));
    }

    #[test]
    fn length_and_incomplete() {
        let (_, message) = run(&[
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
        ]);
        assert_eq!(message.unwrap().stop_reason, StopReason::Length);
        let (_, message) = run(&[
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}}),
        ]);
        assert_eq!(message.unwrap_err().code(), Some("content_filter"));
    }

    #[test]
    fn failed_and_error_events() {
        let (_, message) = run(&[
            json!({"type": "response.failed", "response": {"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}}),
        ]);
        assert_eq!(
            message.unwrap_err().code(),
            Some("subscription_sharing_usage_limit_exceeded")
        );
        let (_, message) =
            run(&[json!({"type": "error", "error": {"code": "rate_limit", "message": "slow"}})]);
        let err = message.unwrap_err();
        assert_eq!(err.code(), Some("rate_limit"));
        assert!(err.to_string().contains("slow"));
    }

    #[test]
    fn invalid_arguments_are_kept_raw() {
        let (_, message) = run(&[
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "function_call", "call_id": "c", "name": "x", "arguments": "{bad"}}),
            completed(json!({})),
        ]);
        assert_eq!(
            message.unwrap().tool_calls().next().unwrap().arguments,
            json!("{bad")
        );
    }
}
