use serde_json::{Map, Value, json};

use crate::types::{AssistantContent, AssistantMessage, CompletionRequest, InputContent, Message};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    ApiKey,
    ChatGpt,
    Codex,
}

const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are a helpful assistant.";

pub fn build_body(provider: &str, flavor: Flavor, request: &CompletionRequest) -> Value {
    let mut input = Vec::new();
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            let mut def = json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            });
            if flavor != Flavor::Codex {
                def["strict"] = Value::Bool(false);
            }
            def
        })
        .collect();
    let tools_in_input = flavor == Flavor::ChatGpt && !tools.is_empty();
    if tools_in_input {
        input.push(json!({
            "type": "additional_tools",
            "role": "developer",
            "tools": tools,
        }));
    }
    convert_messages(provider, &request.model, &request.messages, &mut input);

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("store".into(), Value::Bool(false));
    match (&request.system, flavor) {
        (Some(system), _) if !system.is_empty() => {
            body.insert("instructions".into(), json!(system));
        }
        (_, Flavor::Codex) => {
            body.insert("instructions".into(), json!(DEFAULT_CODEX_INSTRUCTIONS));
        }
        _ => {}
    }
    if !tools.is_empty() {
        if !tools_in_input {
            body.insert("tools".into(), Value::Array(tools));
        }
        body.insert("tool_choice".into(), json!("auto"));
        body.insert("parallel_tool_calls".into(), Value::Bool(true));
    }
    if let Some(session) = &request.session_id {
        let key: String = session.chars().take(64).collect();
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if let Some(max) = request.max_output_tokens
        && flavor == Flavor::ApiKey
    {
        body.insert("max_output_tokens".into(), json!(max.max(16)));
    }
    if let Some(wanted) = request.reasoning {
        // Ask only for an effort the model lists, when it lists them.
        let effort = request
            .capabilities
            .nearest_effort(wanted)
            .unwrap_or(wanted);
        let mut reasoning = json!({ "effort": effort.as_str() });
        if effort != crate::types::ReasoningEffort::None {
            reasoning["summary"] = json!("auto");
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
        body.insert("reasoning".into(), reasoning);
    }
    Value::Object(body)
}

fn convert_messages(provider: &str, model: &str, messages: &[Message], input: &mut Vec<Value>) {
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::User { content } => {
                let content: Vec<Value> = content.iter().map(input_content).collect();
                if !content.is_empty() {
                    input.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant(assistant) => {
                convert_assistant(provider, model, index, assistant, input)
            }
            Message::ToolResult {
                call_id, content, ..
            } => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_output(content),
                }));
            }
        }
    }
}

fn convert_assistant(
    provider: &str,
    model: &str,
    index: usize,
    message: &AssistantMessage,
    input: &mut Vec<Value>,
) {
    let same_provider = message.provider == provider;
    let same_model = same_provider && message.model == model;
    let mut text_index = 0;
    for content in &message.content {
        match content {
            AssistantContent::Reasoning { provider_data, .. } => {
                if same_model
                    && let Some(item) = provider_data
                    && item.get("type").and_then(Value::as_str) == Some("reasoning")
                {
                    input.push(item.clone());
                }
            }
            AssistantContent::Text {
                text,
                provider_data,
            } => {
                let data = provider_data.as_ref().filter(|_| same_provider);
                let field = |key: &str| data.and_then(|d| d.get(key)).and_then(Value::as_str);
                let id = match field("id") {
                    Some(id) if id.len() <= 64 => id.to_owned(),
                    _ if text_index == 0 => format!("msg_loom_{index}"),
                    _ => format!("msg_loom_{index}_{text_index}"),
                };
                text_index += 1;
                let mut item = json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    "status": "completed",
                    "id": id,
                });
                if let Some(phase) = field("phase") {
                    item["phase"] = json!(phase);
                }
                input.push(item);
            }
            AssistantContent::ToolCall(call) => {
                let arguments = match &call.arguments {
                    Value::String(raw) => raw.clone(),
                    other => other.to_string(),
                };
                let mut item = json!({
                    "type": "function_call",
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": arguments,
                });
                let item_id = call
                    .provider_data
                    .as_ref()
                    .and_then(|d| d.get("id"))
                    .and_then(Value::as_str);
                if let Some(id) = item_id
                    && same_model
                    && id.starts_with("fc")
                {
                    item["id"] = json!(id);
                }
                input.push(item);
            }
        }
    }
}

fn input_content(content: &InputContent) -> Value {
    match content {
        InputContent::Text { text } => json!({ "type": "input_text", "text": text }),
        InputContent::Image { mime_type, data } => json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": format!("data:{mime_type};base64,{data}"),
        }),
    }
}

fn tool_output(content: &[InputContent]) -> Value {
    let has_image = content
        .iter()
        .any(|c| matches!(c, InputContent::Image { .. }));
    if has_image {
        return Value::Array(content.iter().map(input_content).collect());
    }
    let text: Vec<&str> = content
        .iter()
        .filter_map(|c| match c {
            InputContent::Text { text } => Some(text.as_str()),
            InputContent::Image { .. } => None,
        })
        .collect();
    let text = text.join("\n");
    if text.trim().is_empty() {
        json!("(no tool output)")
    } else {
        json!(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ReasoningEffort, StopReason, ToolCall, ToolDefinition, Usage};

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "gpt-x".into(),
            system: Some("be brief".into()),
            messages: vec![
                Message::user_text("hi"),
                Message::Assistant(AssistantMessage {
                    provider: "openai".into(),
                    model: "gpt-x".into(),
                    content: vec![
                        AssistantContent::Reasoning {
                            summary: "think".into(),
                            provider_data: Some(
                                json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "enc"}),
                            ),
                        },
                        AssistantContent::Text {
                            text: "calling".into(),
                            provider_data: Some(json!({"id": "msg_1"})),
                        },
                        AssistantContent::ToolCall(ToolCall {
                            call_id: "call_1".into(),
                            name: "read".into(),
                            arguments: json!({"path": "a"}),
                            provider_data: Some(json!({"id": "fc_1"})),
                        }),
                    ],
                    usage: Usage::default(),
                    stop_reason: StopReason::ToolUse,
                    response_id: None,
                }),
                Message::ToolResult {
                    call_id: "call_1".into(),
                    tool_name: "read".into(),
                    content: vec![InputContent::text("file")],
                    is_error: false,
                },
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object"}),
            }],
            reasoning: Some(ReasoningEffort::High),
            max_output_tokens: Some(1000),
            session_id: Some("s".repeat(100)),
            capabilities: Default::default(),
        }
    }

    #[test]
    fn api_key_body() {
        let body = build_body("openai", Flavor::ApiKey, &request());
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["prompt_cache_key"].as_str().unwrap().len(), 64);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["id"], "msg_1");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["id"], "fc_1");
        assert_eq!(input[3]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["output"], "file");
    }

    #[test]
    fn chatgpt_body_omits_unsupported_fields_and_uses_additional_tools() {
        let body = build_body("openai", Flavor::ChatGpt, &request());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("tools").is_none());
        assert_eq!(body["tool_choice"], "auto");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["tools"][0]["name"], "read");
    }

    #[test]
    fn codex_body_requires_instructions() {
        let mut req = request();
        req.system = None;
        let body = build_body("openai-codex", Flavor::Codex, &req);
        assert_eq!(body["instructions"], DEFAULT_CODEX_INSTRUCTIONS);
        assert!(body["tools"][0].get("strict").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn foreign_history_drops_reasoning_and_item_ids() {
        let body = build_body("openai-codex", Flavor::Codex, &request());
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|i| i["type"] != "reasoning"));
        assert_eq!(input[1]["id"], "msg_loom_1");
        assert!(input[2].get("id").is_none());
    }

    #[test]
    fn reasoning_none_skips_summary() {
        let mut req = request();
        req.reasoning = Some(ReasoningEffort::None);
        let body = build_body("openai", Flavor::ApiKey, &req);
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
        assert!(body.get("include").is_none());
    }

    #[test]
    fn reasoning_keeps_to_listed_efforts() {
        let mut req = request();
        req.reasoning = Some(ReasoningEffort::Minimal);
        req.capabilities.efforts = Some(vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
        ]);
        let body = build_body("openai-codex", Flavor::Codex, &req);
        assert_eq!(body["reasoning"]["effort"], "low");
        req.reasoning = Some(ReasoningEffort::Xhigh);
        let body = build_body("openai-codex", Flavor::Codex, &req);
        assert_eq!(body["reasoning"]["effort"], "high");
    }
}
