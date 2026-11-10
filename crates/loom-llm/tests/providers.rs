mod common;

use std::time::Duration;

use common::{Harness, codex_access_token, now, sse_body, text_response_events};
use futures::StreamExt;
use loom_config::{CodexProviderConfig, OpenAiAuth, OpenAiProviderConfig};
use loom_llm::auth::{ChatGptAccount, CodexCredential, OAuthTokens};
use loom_llm::responses::RetryPolicy;
use loom_llm::{
    CodexProvider, CompletionChunk, CompletionRequest, LlmError, LlmProvider, Message,
    OpenAiProvider, ToolDefinition, collect,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, Request, ResponseTemplate};

fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_secs(1),
    }
}

fn sse(events: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse_body(events))
}

fn api_key_provider(h: &Harness) -> OpenAiProvider {
    OpenAiProvider::new(
        h.http.clone(),
        h.auth.clone(),
        &OpenAiProviderConfig {
            auth: OpenAiAuth::ApiKey,
            api_key_env: Some("LOOM_TEST_UNSET_OPENAI_KEY".into()),
            base_url: Some(format!("{}/v1", h.server.uri())),
        },
    )
    .with_retry(fast_retry())
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "gpt-test".into(),
        system: Some("system prompt".into()),
        messages: vec![Message::user_text("hello")],
        tools: vec![ToolDefinition {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }],
        max_output_tokens: Some(512),
        session_id: Some("session-1".into()),
        ..CompletionRequest::default()
    }
}

#[tokio::test]
async fn api_key_streams_text() {
    let h = Harness::new().await;
    h.auth.set_api_key(Some("sk-test".into())).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-test"))
        .and(body_partial_json(json!({
            "model": "gpt-test",
            "store": false,
            "stream": true,
            "instructions": "system prompt",
            "max_output_tokens": 512,
            "tools": [{"type": "function", "name": "read"}],
        })))
        .respond_with(sse(&text_response_events("Hi there")))
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = api_key_provider(&h);
    let mut stream = provider.stream(request()).await.unwrap();
    let mut deltas = String::new();
    let mut done = None;
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            CompletionChunk::TextDelta { delta, .. } => deltas.push_str(&delta),
            CompletionChunk::Done(message) => done = Some(message),
            _ => {}
        }
    }
    let message = done.unwrap();
    assert_eq!(deltas, "Hi there");
    assert_eq!(message.text(), "Hi there");
    assert_eq!(message.provider, "openai");
    assert_eq!(message.usage.total, 12);
}

#[tokio::test]
async fn missing_api_key_is_not_authenticated() {
    let h = Harness::new().await;
    let err = api_key_provider(&h).stream(request()).await.err().unwrap();
    assert!(matches!(err, LlmError::NotAuthenticated { .. }));
}

#[tokio::test]
async fn retries_server_errors() {
    let h = Harness::new().await;
    h.auth.set_api_key(Some("sk-test".into())).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(sse(&text_response_events("ok")))
        .with_priority(2)
        .mount(&h.server)
        .await;
    let message = collect(api_key_provider(&h).stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(message.text(), "ok");
    assert_eq!(h.server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn quota_errors_fail_without_retry() {
    let h = Harness::new().await;
    h.auth.set_api_key(Some("sk-test".into())).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"type": "insufficient_quota", "message": "You exceeded your quota"}
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    let err = api_key_provider(&h).stream(request()).await.err().unwrap();
    match err {
        LlmError::Api {
            status,
            code,
            message,
            ..
        } => {
            assert_eq!(status, 429);
            assert_eq!(code.as_deref(), Some("insufficient_quota"));
            assert!(message.contains("quota"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn stream_failure_is_reported() {
    let h = Harness::new().await;
    h.auth.set_api_key(Some("sk-test".into())).await.unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(sse(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.failed", "response": {"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit reached"}}}),
        ]))
        .mount(&h.server)
        .await;
    let err = collect(api_key_provider(&h).stream(request()).await.unwrap())
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        Some("subscription_sharing_usage_limit_exceeded")
    );
}

async fn store_chatgpt_account(h: &Harness) {
    h.store
        .update(|c| {
            c.openai.upsert(ChatGptAccount {
                client_id: "oaiapp_1".into(),
                issuer: "https://auth.openai.com".into(),
                subject: "user-1".into(),
                email: None,
                id_token: None,
                tokens: Some(OAuthTokens {
                    access_token: "chatgpt-access".into(),
                    refresh_token: "r".into(),
                    expires_at: now() + 3600,
                    scopes: vec!["chatgpt.tokens.use.direct".into()],
                    saved_at: now(),
                }),
            })
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn chatgpt_uses_additional_tools_and_omits_unsupported_fields() {
    let h = Harness::new().await;
    store_chatgpt_account(&h).await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer chatgpt-access"))
        .respond_with(sse(&text_response_events("hey")))
        .mount(&h.server)
        .await;
    let provider = OpenAiProvider::new(
        h.http.clone(),
        h.auth.clone(),
        &OpenAiProviderConfig {
            base_url: Some(format!("{}/v1", h.server.uri())),
            ..OpenAiProviderConfig::default()
        },
    );
    let message = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(message.text(), "hey");
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("tools").is_none());
    assert_eq!(body["input"][0]["type"], "additional_tools");
}

#[tokio::test]
async fn chatgpt_lists_visible_models() {
    let h = Harness::new().await;
    store_chatgpt_account(&h).await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"slug": "gpt-a", "display_name": "GPT A", "visibility": "list"},
                {"slug": "gpt-hidden", "display_name": "Hidden", "visibility": "hide"},
                {"slug": "gpt-b", "display_name": "GPT B", "visibility": "list"},
            ]
        })))
        .mount(&h.server)
        .await;
    let provider = OpenAiProvider::new(
        h.http.clone(),
        h.auth.clone(),
        &OpenAiProviderConfig {
            base_url: Some(format!("{}/v1", h.server.uri())),
            ..OpenAiProviderConfig::default()
        },
    );
    let models = provider.list_models().await.unwrap();
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-a", "gpt-b"]);
    assert_eq!(models[0].display_name, "GPT A");
}

#[tokio::test]
async fn codex_sends_account_headers() {
    let h = Harness::new().await;
    let token = codex_access_token("acct-7");
    let stored = token.clone();
    h.store
        .update(move |c| {
            c.openai_codex = Some(CodexCredential {
                account_id: "acct-7".into(),
                access_token: stored,
                refresh_token: "r".into(),
                expires_at: now() + 3600,
                saved_at: now(),
            })
        })
        .await
        .unwrap();
    let auth_header = format!("Bearer {token}");
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", auth_header.as_str()))
        .and(header("chatgpt-account-id", "acct-7"))
        .and(header("originator", "loom"))
        .and(header("openai-beta", "responses=experimental"))
        .and(header("session-id", "session-1"))
        .and(body_partial_json(
            json!({"instructions": "system prompt", "store": false}),
        ))
        .respond_with(sse(&text_response_events("codex")))
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = CodexProvider::new(
        h.http.clone(),
        h.auth.clone(),
        &CodexProviderConfig {
            base_url: Some(format!("{}/backend-api", h.server.uri())),
            ..CodexProviderConfig::default()
        },
    );
    let message = collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(message.text(), "codex");
    assert_eq!(message.provider, "openai-codex");
    let requests: Vec<Request> = h.server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(body["tools"][0]["name"], "read");
}
