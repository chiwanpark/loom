mod common;

use common::{Harness, id, sse_body, text_response_events};
use loom_llm::{
    CompletionRequest, InputContent, LlmError, LoginMethodInfo, Message, ReasoningEffort,
    StopReason, ToolDefinition, collect,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, header_exists, method, path};
use wiremock::{Mock, ResponseTemplate};

const GO: &str = "opencode-go";
const DEEPSEEK: &str = "deepseek-v4.1-flash";
const CLAUDE: &str = "claude-haiku-5-5";
const GPT: &str = "gpt-6-luna";
const GEMINI: &str = "gemini-4-argon";

async fn catalog(h: &Harness) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": GPT, "type": "chat", "recommended_api": "responses", "name": "GPT-6 Luna", "limit": {"context": 1_050_000, "input": 922_000, "output": 128_000}, "capabilities": {"reasoning": {"supported": true, "effort": ["none", "low", "medium", "high", "xhigh"]}}},
            {"id": DEEPSEEK, "type": "chat", "recommended_api": "chat-completions", "capabilities": {"reasoning": {"supported": true, "effort": ["low", "high", "max"]}}, "api": {"chat-completions": {"compatibility": {"reasoning_field": "reasoning_content"}}}},
            {"id": CLAUDE, "type": "chat", "recommended_api": "messages", "capabilities": {"reasoning": {"supported": true, "toggle": true, "effort": ["low", "medium", "high"]}}},
            {"id": GEMINI, "type": "chat", "recommended_api": "interactions", "capabilities": {"reasoning": {"supported": true, "effort": ["low", "medium", "high"]}}},
            {"id": "embedding", "type": "embedding"},
            {"id": "unknown-metadata"}
        ]})))
        .expect(1)
        .mount(&h.server)
        .await;
}

fn settings(h: &Harness) -> Value {
    json!({"base_url": format!("{}/", h.server.uri())})
}

fn request(model: &str) -> CompletionRequest {
    CompletionRequest {
        model: model.into(),
        messages: vec![Message::user_text("Hello")],
        system: Some("Be brief".into()),
        reasoning: Some(ReasoningEffort::Medium),
        session_id: Some("session-123".into()),
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read file".into(),
            parameters: json!({"type": "object"}),
        }],
        ..Default::default()
    }
}

async fn store_key(h: &Harness) {
    h.auth
        .set_api_key(&id(GO), Some("go-test-key".into()))
        .await
        .unwrap();
}

fn sse(events: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse_body(events))
}

#[tokio::test]
async fn lists_models_and_manages_stored_api_key() {
    let h = Harness::new().await;
    catalog(&h).await;
    let provider = h.provider(GO, settings(&h));
    let status = h.auth.provider_status(&id(GO)).await.unwrap();
    assert_eq!(status.display_name, "OpenCode Go");
    assert_eq!(status.login_methods, vec![LoginMethodInfo::ApiKey]);
    assert!(!status.api_key_set);
    assert!(matches!(
        provider.stream(request(GPT)).await,
        Err(LlmError::NotAuthenticated { provider, message })
            if provider == GO && message.contains("OPENCODE_API_KEY")
    ));

    store_key(&h).await;
    assert!(h.auth.provider_status(&id(GO)).await.unwrap().api_key_set);
    assert_eq!(h.section(GO).await, json!({"api_key": "go-test-key"}));
    let models = provider.list_models().await.unwrap();
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, [CLAUDE, DEEPSEEK, GEMINI, GPT, "unknown-metadata"]);
    let gpt = &models[3];
    assert_eq!(gpt.display_name, "GPT-6 Luna");
    assert_eq!(gpt.capabilities.context_window, Some(922_000));
    assert_eq!(gpt.capabilities.max_output_tokens, Some(128_000));
    assert!(
        gpt.capabilities
            .efforts
            .as_ref()
            .unwrap()
            .contains(&ReasoningEffort::High)
    );
    let requests = h.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));

    h.auth.logout(&id(GO)).await.unwrap();
    assert!(!h.auth.provider_status(&id(GO)).await.unwrap().api_key_set);
}

#[tokio::test]
async fn uses_chat_completions_and_replays_thinking_with_tool_results() {
    let h = Harness::new().await;
    catalog(&h).await;
    store_key(&h).await;
    let provider = h.provider(GO, settings(&h));
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("authorization", "Bearer go-test-key"))
        .and(header("x-opencode-session", "session-123"))
        .and(body_partial_json(json!({"model": DEEPSEEK, "thinking": {"type": "enabled"}, "reasoning_effort": "low", "stream": true})))
        .respond_with(sse(&[
            json!({"id": "chat_1", "choices": [{"delta": {"reasoning_content": null, "reasoning": "think"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]}),
        ]))
        .expect(1)
        .mount(&h.server)
        .await;
    let mut req = request(DEEPSEEK);
    let reply = collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(reply.provider, GO);
    assert_eq!(reply.stop_reason, StopReason::ToolUse);
    assert_eq!(reply.tool_calls().next().unwrap().name, "read");

    req.reasoning = Some(ReasoningEffort::Xhigh);
    req.messages.push(Message::Assistant(reply.clone()));
    req.messages.push(Message::ToolResult {
        call_id: "call_1".into(),
        tool_name: "read".into(),
        content: vec![InputContent::text("file contents")],
        is_error: false,
    });
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header("x-opencode-session", "session-123"))
        .and(body_partial_json(json!({"reasoning_effort": "max"})))
        .respond_with(sse(&[
            json!({"choices": [{"delta": {"content": "Done"}, "finish_reason": "stop"}]}),
        ]))
        .expect(1)
        .mount(&h.server)
        .await;
    let reply = collect(provider.stream(req).await.unwrap()).await.unwrap();
    assert_eq!(reply.text(), "Done");
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(body["messages"][2]["reasoning_content"], "think");
    assert_eq!(body["messages"][3]["tool_call_id"], "call_1");
}

#[tokio::test]
async fn uses_messages_without_claude_code_identity() {
    let h = Harness::new().await;
    catalog(&h).await;
    store_key(&h).await;
    Mock::given(method("POST"))
        .and(path("/anthropic/v1/messages"))
        .and(header("authorization", "Bearer go-test-key"))
        .and(header("x-opencode-session", "session-123"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(json!({"model": CLAUDE, "thinking": {"type": "adaptive"}, "output_config": {"effort": "medium"}})))
        .respond_with(sse(&[
            json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 7}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "Hi"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ]))
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = h.provider(GO, settings(&h));
    let reply = collect(provider.stream(request(CLAUDE)).await.unwrap())
        .await
        .unwrap();
    assert_eq!(reply.provider, GO);
    assert_eq!(reply.text(), "Hi");
    assert_eq!(reply.usage.total, 8);

    let requests = h.server.received_requests().await.unwrap();
    let request = &requests[1];
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert!(!request.headers.contains_key("anthropic-beta"));
    assert!(!request.headers.contains_key("x-app"));
    assert_eq!(body["system"].as_array().unwrap().len(), 1);
    assert_eq!(body["system"][0]["text"], "Be brief");
    assert_eq!(body["tools"][0]["name"], "read");
}

#[tokio::test]
async fn uses_responses_for_native_and_translated_models() {
    let h = Harness::new().await;
    catalog(&h).await;
    store_key(&h).await;
    let provider = h.provider(GO, settings(&h));
    for model in [GPT, GEMINI] {
        Mock::given(method("POST"))
            .and(path("/openai/v1/responses"))
            .and(header("authorization", "Bearer go-test-key"))
            .and(header("x-opencode-session", "session-123"))
            .and(body_partial_json(json!({"model": model, "reasoning": {"effort": "medium"}, "store": false, "stream": true})))
            .respond_with(sse(&text_response_events("Hi")))
            .expect(1)
            .mount(&h.server)
            .await;
        let reply = collect(provider.stream(request(model)).await.unwrap())
            .await
            .unwrap();
        assert_eq!(reply.provider, GO);
        assert_eq!(reply.model, model);
        assert_eq!(reply.text(), "Hi");
    }
}

#[tokio::test]
async fn falls_back_to_chat_completions_when_discovery_fails() {
    let h = Harness::new().await;
    store_key(&h).await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/openai/v1/chat/completions"))
        .and(header_exists("x-opencode-session"))
        .and(body_partial_json(json!({"model": "some-new-model"})))
        .respond_with(sse(&[
            json!({"choices": [{"delta": {"content": "Hi"}, "finish_reason": "stop"}]}),
        ]))
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = h.provider(GO, settings(&h));
    let mut req = request("some-new-model");
    req.session_id = None;
    let reply = collect(provider.stream(req).await.unwrap()).await.unwrap();
    assert_eq!(reply.text(), "Hi");
}
