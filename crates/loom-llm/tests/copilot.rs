mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{COPILOT, Harness, id, now, sse_body, text_response_events};
use loom_llm::{
    CompletionRequest, InputContent, LlmError, LlmProvider, LoginMethod, LoginMethodInfo,
    LoginPrompt, Message, ReasoningEffort, StopReason, ToolDefinition, collect,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, body_string_contains, header, method, path};
use wiremock::{Mock, ResponseTemplate};

const ACCESS: &str = "tid=test;proxy-ep=proxy.individual.githubcopilot.com";
const TOKEN_PATH: &str = "/copilot_internal/v2/token";
const DEVICE_POLL: &str = "/login/oauth/access_token";

fn provider(h: &Harness) -> Arc<dyn LlmProvider> {
    h.provider(COPILOT, json!({"base_url": h.server.uri()}))
}

async fn credential(h: &Harness, expires_at: u64) {
    h.set_section(
        COPILOT,
        json!({
            "github_token": "github-token", "token": ACCESS, "expires_at": expires_at,
        }),
    )
    .await;
}

async fn mount_exchange(h: &Harness, token: &str) {
    Mock::given(method("GET"))
        .and(path(TOKEN_PATH))
        .and(header("authorization", "Bearer github-token"))
        .and(header("copilot-integration-id", "vscode-chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token": token, "expires_at": now() + 3600,
        })))
        .expect(1)
        .mount(&h.server)
        .await;
}

async fn mount_device(h: &Harness, verification_uri: &str, expires_in: u64) {
    Mock::given(method("POST"))
        .and(path("/login/device/code"))
        .and(body_string_contains("client_id=Iv1.b507a08c87ecfe98"))
        .and(body_string_contains("scope=read%3Auser"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "private-code", "user_code": "ABCD-1234", "interval": 1,
            "verification_uri": verification_uri, "expires_in": expires_in,
        })))
        .mount(&h.server)
        .await;
}

#[tokio::test]
async fn device_login_exchanges_and_stores_credentials_until_logout() {
    let h = Harness::new().await;
    let copilot = id(COPILOT);
    let status = h.auth.provider_status(&copilot).await.unwrap();
    assert_eq!(status.display_name, "GitHub Copilot");
    assert_eq!(status.login_methods, [LoginMethodInfo::DeviceCode]);
    assert!(!status.signed_in());
    assert!(matches!(
        provider(&h).list_models().await,
        Err(LlmError::NotAuthenticated { .. })
    ));
    mount_device(&h, "https://github.com/login/device", 900).await;
    Mock::given(method("POST"))
        .and(path(DEVICE_POLL))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"error": "authorization_pending"})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path(DEVICE_POLL))
        .and(body_string_contains("device_code=private-code"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "github-token"})),
        )
        .with_priority(2)
        .expect(1)
        .mount(&h.server)
        .await;
    mount_exchange(&h, ACCESS).await;
    let login = h
        .auth
        .start_login(&copilot, LoginMethod::DeviceCode)
        .await
        .unwrap();
    assert_eq!(
        login.prompt,
        LoginPrompt::DeviceCode {
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            expires_in_secs: 900,
        }
    );
    let info = h
        .auth
        .wait_login(&login.login_id, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.provider, COPILOT);
    assert!(h.auth.provider_status(&copilot).await.unwrap().signed_in());
    let stored = h.section(COPILOT).await;
    assert_eq!(stored["github_token"], "github-token");
    assert_eq!(stored["token"], ACCESS);
    h.auth.logout(&copilot).await.unwrap();
    assert!(!h.auth.provider_status(&copilot).await.unwrap().signed_in());
    assert_eq!(h.section(COPILOT).await, Value::Null);
}

#[tokio::test]
async fn device_login_honors_slow_down_and_expiry() {
    let h = Harness::new().await;
    mount_device(&h, "https://github.com/login/device", 4).await;
    Mock::given(method("POST"))
        .and(path(DEVICE_POLL))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"error": "slow_down"})))
        .expect(1)
        .mount(&h.server)
        .await;
    let login = h
        .auth
        .start_login(&id(COPILOT), LoginMethod::DeviceCode)
        .await
        .unwrap();
    let error = h
        .auth
        .wait_login(&login.login_id, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Login(message) if message.contains("expired")));
    assert_eq!(h.section(COPILOT).await, Value::Null);
}

#[tokio::test]
async fn device_login_can_be_cancelled_during_exchange() {
    let h = Harness::new().await;
    mount_device(&h, "https://github.com/login/device", 900).await;
    Mock::given(method("POST"))
        .and(path(DEVICE_POLL))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "github-token"})),
        )
        .mount(&h.server)
        .await;
    let mock = Mock::given(method("GET"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .expect(1)
        .mount_as_scoped(&h.server)
        .await;
    let login = h
        .auth
        .start_login(&id(COPILOT), LoginMethod::DeviceCode)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (result, ()) = tokio::join!(h.auth.wait_login(&login.login_id, &cancel), async {
        mock.wait_until_satisfied().await;
        cancel.cancel();
    });
    assert!(matches!(result, Err(LlmError::Cancelled)));
    assert_eq!(h.section(COPILOT).await, Value::Null);
}

#[tokio::test]
async fn device_login_rejects_untrusted_verification_urls() {
    let h = Harness::new().await;
    mount_device(&h, "https://github.com@attacker.example/login/device", 900).await;
    let err = h
        .auth
        .start_login(&id(COPILOT), LoginMethod::DeviceCode)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Login(message) if message.contains("verification URL")));
}

#[tokio::test]
async fn discovery_refreshes_once_and_uses_account_models_and_limits() {
    let h = Harness::new().await;
    credential(&h, 1).await;
    mount_exchange(&h, "new-copilot-token").await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer new-copilot-token"))
        .and(header("x-github-api-version", "2026-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
            {"id": "unconfigured", "policy": {"state": "unconfigured"}},
            {"id": "disabled", "model_picker_enabled": true, "policy": {"state": "disabled"}},
            {"id": "hidden", "model_picker_enabled": false, "policy": {"state": "enabled"}},
            {"id": "no-tools", "capabilities": {"supports": {"tool_calls": false}}},
            {"id": "no-streaming", "capabilities": {"supports": {"streaming": false}}},
            {"id": "embedding", "capabilities": {"type": "embeddings"}},
            {"id": "classifier", "supported_endpoints": ["/v1/systemone"]},
            {"id": "gpt-4.1", "model_picker_enabled": true, "name": "GPT-4.1",
                "supported_endpoints": ["/chat/completions"],
                "capabilities": {"type": "chat",
                    "limits": {"max_prompt_tokens": 100_000, "max_output_tokens": 10_000},
                    "supports": {"reasoning_effort": ["medium", "high"]}}
            },
        ]})))
        .expect(2)
        .mount(&h.server)
        .await;
    let provider = provider(&h);
    let (first, second) = tokio::join!(provider.list_models(), provider.list_models());
    let models = first.unwrap();
    assert_eq!(models, second.unwrap());
    assert_eq!(models.len(), 1);
    let model = &models[0];
    assert_eq!(model.display_name, "GPT-4.1");
    assert_eq!(model.capabilities.context_window, Some(100_000));
    assert_eq!(model.capabilities.max_output_tokens, Some(10_000));
    assert_eq!(
        model.capabilities.efforts,
        Some(vec![ReasoningEffort::Medium, ReasoningEffort::High])
    );
    assert_eq!(h.section(COPILOT).await["token"], "new-copilot-token");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer new-copilot-token"))
        .and(body_json(json!({
            "model": "gpt-4.1", "stream": true, "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "hi"}], "reasoning_effort": "high",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"id\":\"chat_1\",\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        )).expect(1).mount(&h.server).await;
    let result = collect(
        provider
            .stream(CompletionRequest {
                model: model.id.clone(),
                capabilities: model.capabilities.clone(),
                reasoning: Some(ReasoningEffort::Xhigh),
                messages: vec![Message::user_text("hi")],
                ..Default::default()
            })
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(result.provider, COPILOT);
    assert_eq!(result.text(), "Hello");
}

fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object"}),
    }
}

#[tokio::test]
async fn streams_responses_with_vision_and_user_initiator() {
    let h = Harness::new().await;
    credential(&h, now() + 3600).await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", format!("Bearer {ACCESS}").as_str()))
        .and(header("x-initiator", "user"))
        .and(header("openai-intent", "conversation-edits"))
        .and(header("copilot-vision-request", "true"))
        .and(body_json(json!({
            "model": "gpt-5.3-codex", "stream": true, "store": false,
            "input": [{"role": "user", "content": [{"type": "input_image", "detail": "auto", "image_url": "data:image/png;base64,AQ=="}]}],
            "tools": [{"type": "function", "name": "read", "description": "Read a file", "parameters": {"type": "object"}, "strict": false}],
            "tool_choice": "auto", "parallel_tool_calls": true,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse_body(&text_response_events("Hello from Copilot")), "text/event-stream",
        )).expect(1).mount(&h.server).await;
    let result = collect(
        provider(&h)
            .stream(CompletionRequest {
                model: "gpt-5.3-codex".into(),
                reasoning: Some(ReasoningEffort::None),
                messages: vec![Message::User {
                    content: vec![InputContent::Image {
                        mime_type: "image/png".into(),
                        data: "AQ==".into(),
                    }],
                }],
                tools: vec![tool()],
                ..Default::default()
            })
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(result.provider, COPILOT);
    assert_eq!(result.text(), "Hello from Copilot");
}

#[tokio::test]
async fn streams_claude_tool_calls_and_replays_signed_thinking_without_claude_code_identity() {
    let h = Harness::new().await;
    credential(&h, now() + 3600).await;
    let mock = || {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("copilot-integration-id", "vscode-chat"))
    };
    mock().and(header("x-initiator", "user"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body(&[
            json!({"type": "message_start", "message": {"id": "msg_1"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "think", "signature": "signed"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "name": "read", "id": "toolu_x", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"README.md\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
            json!({"type": "message_stop"}),
        ]), "text/event-stream")).expect(1).mount(&h.server).await;
    mock().and(header("x-initiator", "agent"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body(&[
            json!({"type": "message_start", "message": {"id": "msg_2"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "Done"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ]), "text/event-stream")).expect(1).mount(&h.server).await;
    let provider = provider(&h);
    let mut request = CompletionRequest {
        model: "claude-sonnet-4.6".into(),
        system: Some("Be brief".into()),
        reasoning: Some(ReasoningEffort::Medium),
        messages: vec![Message::user_text("hi")],
        tools: vec![tool()],
        ..Default::default()
    };
    let first = collect(provider.stream(request.clone()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(first.provider, COPILOT);
    assert_eq!(first.stop_reason, StopReason::ToolUse);
    let call = first.tool_calls().next().unwrap();
    assert_eq!(call.arguments, json!({"path": "README.md"}));
    let output = Message::ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        content: vec![InputContent::text("file content")],
        is_error: false,
    };
    request.messages.extend([Message::Assistant(first), output]);
    let second = collect(provider.stream(request).await.unwrap())
        .await
        .unwrap();
    assert_eq!(second.text(), "Done");
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(
        body["system"],
        json!([{"type": "text", "text": "Be brief", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["messages"][1]["content"][0]["signature"], "signed");
    assert_eq!(body["messages"][1]["content"][1]["name"], "read");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_x");
    assert!(!requests[1].headers.contains_key("anthropic-beta"));
}
