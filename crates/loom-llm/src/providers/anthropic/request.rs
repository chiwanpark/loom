use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{CLAUDE_CODE_VERSION, ID};
use crate::error::{LlmError, Result};
use crate::types::{AssistantContent, CompletionRequest, InputContent, Message, ReasoningEffort};

/// Ask for the summarized thinking text. Newer models default to `omitted`,
/// which returns only a signature and leaves nothing to show in the chat.
const THINKING_DISPLAY: &str = "summarized";

pub(super) fn tool_name(name: &str) -> String {
    const NAMES: &[&str] = &[
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "KillShell",
        "NotebookEdit",
        "Skill",
        "Task",
        "TaskOutput",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
    ];
    NAMES
        .iter()
        .find(|n| n.eq_ignore_ascii_case(name))
        .copied()
        .unwrap_or(name)
        .into()
}

fn input(content: &[InputContent]) -> Vec<Value> {
    content.iter().map(|c| match c {
        InputContent::Text { text } => json!({"type": "text", "text": text}),
        InputContent::Image { mime_type, data } => json!({
            "type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data},
        }),
    }).collect()
}

fn append(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == role) {
        last["content"].as_array_mut().unwrap().extend(content);
    } else {
        messages.push(json!({"role": role, "content": content}));
    }
}

fn billing_header(text: &str) -> String {
    let cch: String = Sha256::digest(text.as_bytes())
        .iter()
        .take(3)
        .map(|b| format!("{b:02x}"))
        .collect();
    let units: Vec<u16> = text.encode_utf16().take(21).collect();
    let sample: Vec<u16> = [4, 7, 20]
        .iter()
        .map(|i| units.get(*i).copied().unwrap_or(b'0' as u16))
        .collect();
    let salted = format!(
        "59cf53e54c78{}{}",
        String::from_utf16_lossy(&sample),
        CLAUDE_CODE_VERSION
    );
    let suffix: String = Sha256::digest(salted.as_bytes())
        .iter()
        .take(2)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION}.{}; cc_entrypoint=sdk-cli; cch={};",
        &suffix[..3],
        &cch[..5]
    )
}

pub(super) fn build_body(request: &CompletionRequest) -> Result<Value> {
    let mut messages = Vec::new();
    for message in &request.messages {
        match message {
            Message::User { content } => append(&mut messages, "user", input(content)),
            Message::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => {
                append(
                    &mut messages,
                    "user",
                    vec![json!({
                        "type": "tool_result", "tool_use_id": call_id,
                        "content": input(content), "is_error": is_error,
                    })],
                );
            }
            Message::Assistant(message) => {
                let same_model = message.provider == ID && message.model == request.model;
                let content = message.content.iter().filter_map(|c| match c {
                    AssistantContent::Text { text, .. } if !text.is_empty() => Some(json!({"type": "text", "text": text})),
                    AssistantContent::Reasoning { provider_data: Some(data), .. } if same_model => {
                        match data["type"].as_str() {
                            Some("thinking") if data["signature"].as_str().is_some_and(|s| !s.is_empty()) => Some(data.clone()),
                            Some("redacted_thinking") => Some(data.clone()),
                            _ => None,
                        }
                    }
                    AssistantContent::ToolCall(call) => Some(json!({
                        "type": "tool_use", "id": call.call_id, "name": tool_name(&call.name), "input": call.arguments,
                    })),
                    _ => None,
                }).collect();
                append(&mut messages, "assistant", content);
            }
        }
    }
    let first_text = messages
        .iter()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_array())
        .and_then(|c| c.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str());
    let mut system = Vec::new();
    if let Some(text) = first_text.filter(|s| !s.is_empty()) {
        system.push(json!({"type": "text", "text": billing_header(text)}));
    }
    system.push(json!({"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}));
    if let Some(text) = request.system.as_deref().filter(|s| !s.is_empty()) {
        system.push(json!({"type": "text", "text": text}));
    }
    system.last_mut().unwrap()["cache_control"] = json!({"type": "ephemeral"});
    if let Some(last) = messages.last_mut().filter(|m| m["role"] == "user")
        && let Some(block) = last["content"].as_array_mut().and_then(|c| c.last_mut())
    {
        block["cache_control"] = json!({"type": "ephemeral"});
    }
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "system": system,
        "max_tokens": request.max_output_tokens.unwrap_or(16384),
        "stream": true,
    });
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request.tools.iter().map(|t| json!({
            "name": tool_name(&t.name), "description": t.description, "input_schema": t.parameters,
        })).collect();
        tools.last_mut().unwrap()["cache_control"] = json!({"type": "ephemeral"});
        body["tools"] = tools.into();
    }
    if let Some(effort) = request.reasoning {
        if effort == ReasoningEffort::None {
            body["thinking"] = json!({"type": "disabled"});
        } else if request.model.contains("4-6")
            || request.model.contains("4-7")
            || request.model.contains("4.6")
            || request.model.contains("4.7")
        {
            body["thinking"] = json!({"type": "adaptive", "display": THINKING_DISPLAY});
            let effort = match effort {
                ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                _ => "high",
            };
            body["output_config"] = json!({"effort": effort});
        } else {
            let budget = match effort {
                ReasoningEffort::Minimal | ReasoningEffort::Low => 1024,
                ReasoningEffort::Medium => 4096,
                ReasoningEffort::High => 8192,
                ReasoningEffort::Xhigh => 16384,
                ReasoningEffort::None => unreachable!(),
            };
            let max = request
                .max_output_tokens
                .unwrap_or(16384.max(budget + 4096));
            if max <= 1024 {
                return Err(LlmError::Unsupported(
                    "Anthropic thinking requires max_output_tokens greater than 1024".into(),
                ));
            }
            body["max_tokens"] = max.into();
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget.min(max - 1),
                "display": THINKING_DISPLAY,
            });
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_budgets_respect_output_limits() {
        let mut request = CompletionRequest {
            model: "claude-sonnet-4-5".into(),
            reasoning: Some(ReasoningEffort::Xhigh),
            ..Default::default()
        };
        let body = build_body(&request).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 16384);
        assert_eq!(body["max_tokens"], 20480);
        request.max_output_tokens = Some(2048);
        assert_eq!(
            build_body(&request).unwrap()["thinking"]["budget_tokens"],
            2047
        );
        request.max_output_tokens = Some(1024);
        assert!(build_body(&request).is_err());
        request.reasoning = Some(ReasoningEffort::None);
        assert_eq!(
            build_body(&request).unwrap()["thinking"]["type"],
            "disabled"
        );
        request.reasoning = None;
        assert!(build_body(&request).unwrap().get("thinking").is_none());
    }

    /// Newer models omit the thinking text by default and return only a
    /// signature, which leaves nothing to show in the chat.
    #[test]
    fn thinking_asks_for_summarized_text() {
        for model in ["claude-sonnet-4-5", "claude-opus-4-7", "claude-opus-5-5"] {
            let request = CompletionRequest {
                model: model.into(),
                reasoning: Some(ReasoningEffort::High),
                ..Default::default()
            };
            let body = build_body(&request).unwrap();
            assert_eq!(body["thinking"]["display"], "summarized", "{model}");
        }
    }
}
