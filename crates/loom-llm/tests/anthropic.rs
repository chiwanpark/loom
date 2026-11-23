mod common;

use std::collections::HashMap;

use base64::Engine;
use common::{Harness, id, now, sse_body};
use futures::StreamExt;
use loom_llm::{
    AccountIdentity, AssistantContent, CompletionChunk, CompletionRequest, InputContent, LlmError,
    LoginInput, LoginMethod, LoginPrompt, Message, ReasoningEffort, StopReason, ToolDefinition,
    collect,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, Request, ResponseTemplate};

const ID: &str = "anthropic";
const MODEL: &str = "claude-sonnet-4-5";
const REDIRECT: &str = "http://localhost:53692/callback";

async fn harness() -> Harness {
    let mut h = Harness::new().await;
    h.auth = h.auth.clone().with_auth_base_url(ID, h.server.uri());
    h
}

async fn credential(h: &Harness, expires: u64) {
    h.set_section(ID, json!({
        "access_token": "sk-ant-oat-access", "refresh_token": "refresh-old", "expires_at": expires,
        "account_id": "account-1", "label": "me@example.com",
    })).await;
}

fn request() -> CompletionRequest {
    CompletionRequest {
        model: MODEL.into(),
        messages: vec![Message::user_text("Hello world")],
        ..Default::default()
    }
}

fn text_events() -> Vec<Value> {
    vec![
        json!({"type": "message_start", "message": {"id": "msg-1", "model": "resolved-model", "usage": {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 4, "cache_creation_input_tokens": 3}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hello"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ]
}

async fn serve_stream(h: &Harness, events: &[Value]) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer sk-ant-oat-access"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("x-app", "cli"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(events)),
        )
        .mount(&h.server)
        .await;
}

#[tokio::test]
async fn browser_login_uses_pkce_and_persists_account() {
    let h = harness().await;
    let started = h
        .auth
        .start_login(
            &id(ID),
            LoginMethod::Browser {
                redirect_uri: Some(REDIRECT.into()),
                account: None,
            },
        )
        .await
        .unwrap();
    let LoginPrompt::OpenUrl { url, redirect_uri } = started.prompt else {
        panic!()
    };
    assert_eq!(redirect_uri, REDIRECT);
    let url = Url::parse(&url).unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(params["scope"].contains("user:inference"));
    let verifier = &params["state"];
    assert_eq!(
        params["code_challenge"],
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()))
    );
    Mock::given(method("POST")).and(path("/v1/oauth/token"))
        .and(body_json(json!({
            "grant_type": "authorization_code", "client_id": params["client_id"], "code": "auth-code",
            "state": verifier, "redirect_uri": REDIRECT, "code_verifier": verifier,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access", "refresh_token": "refresh", "expires_in": 3600,
            "account": {"uuid": "account-1", "email_address": "me@example.com"},
            "organization": {"uuid": "org-1", "name": "Example"},
        }))).expect(1).mount(&h.server).await;
    let account = h
        .auth
        .complete_login(
            &started.login_id,
            LoginInput::Redirect {
                url: format!("{REDIRECT}?code=auth-code&state={verifier}"),
            },
        )
        .await
        .unwrap();
    assert_eq!(account.account_id.as_deref(), Some("account-1"));
    assert_eq!(account.label.as_deref(), Some("me@example.com"));
    assert_eq!(h.section(ID).await["access_token"], "access");
    let status = h.auth.provider_status(&id(ID)).await.unwrap();
    assert!(status.signed_in());
    assert!(!status.api_key_set);
    assert_eq!(
        h.auth.account_identity(&id(ID)).await.unwrap(),
        Some(AccountIdentity {
            email: Some("me@example.com".into()),
            account_uuid: Some("account-1".into()),
            organization_uuid: Some("org-1".into()),
        })
    );
    h.auth.logout(&id(ID)).await.unwrap();
    assert!(h.section(ID).await.is_null());
    assert!(!h.auth.provider_status(&id(ID)).await.unwrap().signed_in());
    assert_eq!(h.auth.account_identity(&id(ID)).await.unwrap(), None);
}

#[tokio::test]
async fn copy_code_login_and_state_validation() {
    let h = harness().await;
    for wrong in [false, true] {
        let started = h
            .auth
            .start_login(
                &id(ID),
                LoginMethod::Browser {
                    redirect_uri: None,
                    account: None,
                },
            )
            .await
            .unwrap();
        let LoginPrompt::OpenUrl { url, redirect_uri } = started.prompt else {
            panic!()
        };
        assert_eq!(
            redirect_uri,
            "https://platform.claude.com/oauth/code/callback"
        );
        let params: HashMap<_, _> = Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        if !wrong {
            Mock::given(path("/v1/oauth/token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "access", "refresh_token": "refresh", "expires_in": 3600})))
                .expect(1).mount(&h.server).await;
        }
        let state = if wrong { "wrong" } else { &params["state"] };
        let result = h
            .auth
            .complete_login(
                &started.login_id,
                LoginInput::Code {
                    code: format!("code#{state}"),
                },
            )
            .await;
        if wrong {
            assert!(result.unwrap_err().to_string().contains("state mismatch"));
        } else {
            assert!(result.is_ok());
        }
    }
    assert!(
        h.auth
            .start_login(&id(ID), LoginMethod::DeviceCode)
            .await
            .is_err()
    );
    assert!(
        h.auth
            .set_api_key(&id(ID), Some("key".into()))
            .await
            .is_err()
    );
    for redirect in [
        "https://attacker.test/callback",
        "http://localhost:53692/wrong",
        "http://localhost:53692/callback?x=1",
    ] {
        assert!(
            h.auth
                .start_login(
                    &id(ID),
                    LoginMethod::Browser {
                        redirect_uri: Some(redirect.into()),
                        account: None
                    }
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn rejects_bad_callbacks_without_exchanging_tokens() {
    let h = harness().await;
    for callback in [
        "http://localhost:53692/callback?code=c",
        "http://localhost:53692/callback?code=c&state=wrong",
        "http://evil.test/callback?code=c&state=wrong",
        "",
        "code#wrong",
    ] {
        let started = h
            .auth
            .start_login(
                &id(ID),
                LoginMethod::Browser {
                    redirect_uri: Some(REDIRECT.into()),
                    account: None,
                },
            )
            .await
            .unwrap();
        assert!(
            h.auth
                .complete_login(
                    &started.login_id,
                    LoginInput::Redirect {
                        url: callback.into()
                    }
                )
                .await
                .is_err()
        );
    }
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn refreshes_once_for_concurrent_requests_and_rotates_tokens() {
    let h = harness().await;
    credential(&h, now() + 100).await;
    Mock::given(method("POST")).and(path("/v1/oauth/token"))
        .and(body_json(json!({"grant_type": "refresh_token", "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e", "refresh_token": "refresh-old"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "sk-ant-oat-access", "refresh_token": "refresh-new", "expires_in": 3600})))
        .expect(1).mount(&h.server).await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(
        ID,
        json!({"base_url": h.server.uri(), "auth": "anthropic_oauth"}),
    );
    let (a, b) = tokio::join!(provider.stream(request()), provider.stream(request()));
    assert_eq!(collect(a.unwrap()).await.unwrap().text(), "Hello");
    assert_eq!(collect(b.unwrap()).await.unwrap().text(), "Hello");
    let stored = h.section(ID).await;
    assert_eq!(stored["refresh_token"], "refresh-new");
    assert_eq!(stored["account_id"], "account-1");
    assert_eq!(stored["label"], "me@example.com");
    assert!(stored["expires_at"].as_u64().unwrap() >= now() + 3500);
}

#[tokio::test]
async fn refresh_failure_preserves_credentials() {
    let h = harness().await;
    credential(&h, 0).await;
    let before = h.section(ID).await;
    Mock::given(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error": "invalid_grant", "error_description": "expired refresh token"}),
        ))
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let error = match provider.stream(request()).await {
        Ok(_) => panic!(),
        Err(e) => e,
    };
    assert!(matches!(error, LlmError::NotAuthenticated { .. }));
    assert_eq!(before, h.section(ID).await);
}

#[tokio::test]
async fn text_stream_has_chunks_usage_and_oauth_shaping() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(
        ID,
        json!({"base_url": format!("{}/v1/messages", h.server.uri())}),
    );
    let chunks: Vec<_> = provider
        .stream(request())
        .await
        .unwrap()
        .map(|c| c.unwrap())
        .collect()
        .await;
    assert!(
        matches!(&chunks[0], CompletionChunk::Start { response_id: Some(id) } if id == "msg-1")
    );
    assert!(
        matches!(&chunks[2], CompletionChunk::TextDelta { index: 0, delta } if delta == "Hello")
    );
    let CompletionChunk::Done(message) = chunks.last().unwrap() else {
        panic!()
    };
    assert_eq!(message.text(), "Hello");
    assert_eq!(message.model, MODEL);
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 2);
    assert_eq!(message.usage.cache_read, 4);
    assert_eq!(message.usage.cache_write, 3);
    assert_eq!(message.usage.total, 19);
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["system"][0]["text"],
        "x-anthropic-billing-header: cc_version=2.1.323.7c0; cc_entrypoint=sdk-cli; cch=64ec8;"
    );
    assert_eq!(
        body["system"][1]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert!(
        requests[0].headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("oauth-2025-04-20")
    );
    assert_eq!(requests[0].headers["user-agent"], "claude-cli/2.1.323");
    assert!(!requests[0].headers.contains_key("x-api-key"));
}

fn thread_request() -> CompletionRequest {
    CompletionRequest {
        session_id: Some("session".into()),
        system: Some("Keep my project instructions.".into()),
        tools: vec![ToolDefinition {
            name: "bash".into(),
            description: "Run a command".into(),
            parameters: json!({"type": "object"}),
        }],
        ..request()
    }
}

fn payload(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn rejection(code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"type": "error", "error": {
        "type": "invalid_request_error", "message": message, "details": {"error_code": code},
    }}))
}

#[tokio::test]
async fn continuation_sends_only_new_messages_and_the_original_billing_block() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(
        ID,
        json!({"base_url": h.server.uri(), "message_threads": true}),
    );
    let mut req = thread_request();
    let reply = collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    req.messages.push(Message::Assistant(reply));
    req.messages.push(Message::user_text("And now?"));
    let reply = collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    req.messages.push(Message::Assistant(reply));
    req.messages.push(Message::user_text("One more question."));
    req.system = Some("New project instructions.".into());
    collect(provider.stream(req).await.unwrap()).await.unwrap();

    let sent = h.server.received_requests().await.unwrap();
    let [create, continued, changed] = sent.as_slice() else {
        panic!("expected three requests")
    };
    assert!(
        create.headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .split(',')
            .any(|beta| beta == "message-threads-2026-08-12")
    );
    let (create, continued, changed) = (payload(create), payload(continued), payload(changed));
    assert_eq!(create["thread"], json!({"type": "create"}));
    assert_eq!(create["system"][2]["text"], "Keep my project instructions.");
    assert_eq!(create["tools"][0]["name"], "Bash");
    assert_eq!(
        continued["thread"],
        json!({"type": "continue", "previous_message_id": "msg-1"})
    );
    assert_eq!(continued["system"], json!([create["system"][0]]));
    assert!(continued.get("tools").is_none());
    assert_eq!(
        continued["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "And now?", "cache_control": {"type": "ephemeral"}}
        ]}])
    );
    assert_eq!(changed["thread"], json!({"type": "create"}));
    assert_eq!(changed["system"][2]["text"], "New project instructions.");
    assert_eq!(changed["messages"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn expired_thread_restarts_with_full_history() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/v1/messages"))
        .and(|req: &Request| payload(req)["thread"]["type"] == "continue")
        .respond_with(rejection("thread_not_found", "The thread expired."))
        .with_priority(1)
        .expect(1)
        .mount(&h.server)
        .await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(
        ID,
        json!({"base_url": h.server.uri(), "message_threads": true}),
    );
    let mut req = thread_request();
    let reply = collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    req.messages.push(Message::Assistant(reply));
    req.messages.push(Message::user_text("And now?"));
    collect(provider.stream(req).await.unwrap()).await.unwrap();
    let sent = h.server.received_requests().await.unwrap();
    assert_eq!(sent.len(), 3);
    let restarted = payload(&sent[2]);
    assert_eq!(restarted["thread"], json!({"type": "create"}));
    assert_eq!(restarted["system"], payload(&sent[0])["system"]);
    assert_eq!(restarted["tools"], payload(&sent[0])["tools"]);
    assert_eq!(restarted["messages"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn unsupported_endpoint_falls_back_without_thread_fields() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/v1/messages"))
        .and(|req: &Request| payload(req).get("thread").is_some())
        .respond_with(rejection(
            "thread_unsupported_request",
            "Threads are not supported.",
        ))
        .with_priority(1)
        .expect(1)
        .mount(&h.server)
        .await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(
        ID,
        json!({"base_url": h.server.uri(), "message_threads": true}),
    );
    for _ in 0..2 {
        collect(provider.stream(thread_request()).await.unwrap())
            .await
            .unwrap();
    }
    let plain = h.provider(ID, json!({"base_url": h.server.uri()}));
    collect(plain.stream(thread_request()).await.unwrap())
        .await
        .unwrap();
    let sent = h.server.received_requests().await.unwrap();
    assert_eq!(sent.len(), 4);
    assert_eq!(payload(&sent[0])["thread"]["type"], "create");
    for req in &sent[1..] {
        assert!(payload(req).get("thread").is_none());
        assert!(
            !req.headers["anthropic-beta"]
                .to_str()
                .unwrap()
                .contains("message-threads")
        );
        assert_eq!(payload(req)["system"], payload(&sent[0])["system"]);
    }
}

#[tokio::test]
async fn retries_a_continuation_at_the_required_billing_version_and_remembers_it() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/v1/messages"))
        .and(|req: &Request| {
            let body = payload(req);
            body["thread"]["type"] == "continue"
                && body["system"][0]["text"].as_str().is_some_and(|t| t.contains("cc_version=2.1.323."))
        })
        .respond_with(rejection("claude_code_version_too_old", "Claude Code 2.1.323 does not support this model; version 2.1.410 or newer is required."))
        .with_priority(1)
        .expect(1)
        .mount(&h.server).await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(
        ID,
        json!({"base_url": h.server.uri(), "message_threads": true}),
    );
    let mut req = thread_request();
    let reply = collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    req.messages.push(Message::Assistant(reply));
    req.messages.push(Message::user_text("And now?"));
    collect(provider.stream(req).await.unwrap()).await.unwrap();
    collect(provider.stream(request()).await.unwrap())
        .await
        .unwrap();
    let sent = h.server.received_requests().await.unwrap();
    assert_eq!(sent.len(), 4);
    let mut expected = payload(&sent[1]);
    expected["system"][0]["text"] =
        "x-anthropic-billing-header: cc_version=2.1.410.74e; cc_entrypoint=sdk-cli; cch=64ec8;"
            .into();
    assert_eq!(payload(&sent[2]), expected);
    assert_eq!(payload(&sent[3])["system"][0], expected["system"][0]);
}

#[tokio::test]
async fn version_recovery_retries_only_once() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/v1/messages"))
        .respond_with(|req: &Request| {
            let body = payload(req);
            let billing = body["system"][0]["text"].as_str().unwrap();
            let required = if billing.contains("cc_version=2.1.323.") {
                "2.1.410"
            } else {
                "2.1.411"
            };
            rejection(
                "claude_code_version_too_old",
                &format!("Claude Code version {required} or newer is required."),
            )
        })
        .expect(2)
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let Err(LlmError::Api { message, .. }) = provider.stream(request()).await else {
        panic!("expected the second API rejection");
    };
    assert!(message.contains("version 2.1.411 or newer is required"));
    assert!(message.contains("Loom sent billing version 2.1.410"));
}

#[tokio::test]
async fn version_rejection_without_a_billing_block_explains_the_missing_text() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/v1/messages"))
        .respond_with(rejection(
            "claude_code_version_too_old",
            "A newer version is required; version 2.1.410 or newer is required.",
        ))
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let req = CompletionRequest {
        messages: vec![Message::User {
            content: vec![InputContent::Image {
                mime_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        }],
        ..request()
    };
    let Err(LlmError::Api {
        message,
        status: 400,
        ..
    }) = provider.stream(req).await
    else {
        panic!("expected the API rejection");
    };
    assert!(message.contains("first user message has no text"));
}

#[tokio::test]
async fn signed_thinking_tools_images_and_history_round_trip() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    let events = vec![
        json!({"type": "message_start", "message": {"id": "msg-2"}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Think"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "signed"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "-data"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "opaque"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "call-1", "name": "Bash", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"pwd\"}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        json!({"type": "message_stop"}),
    ];
    serve_stream(&h, &events).await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let mut req = request();
    req.system = Some("Keep my project instructions.".into());
    req.tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "Run command".into(),
        parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
    }];
    req.reasoning = Some(ReasoningEffort::Medium);
    let message = collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert!(
        matches!(&message.content[0], AssistantContent::Reasoning { summary, provider_data: Some(data) } if summary == "Think" && data["signature"] == "signed-data")
    );
    let call = message.tool_calls().next().unwrap();
    assert_eq!(call.name, "bash");
    assert_eq!(call.arguments, json!({"command": "pwd"}));
    req.messages.push(Message::Assistant(message));
    req.messages.push(Message::ToolResult {
        call_id: "call-1".into(),
        tool_name: "bash".into(),
        content: vec![InputContent::text("/project")],
        is_error: false,
    });
    req.messages.push(Message::User {
        content: vec![InputContent::Image {
            mime_type: "image/png".into(),
            data: "aGVsbG8=".into(),
        }],
    });
    collect(provider.stream(req.clone()).await.unwrap())
        .await
        .unwrap();
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(body["tools"][0]["name"], "Bash");
    assert_eq!(body["system"][2]["text"], "Keep my project instructions.");
    assert_eq!(
        body["messages"][1]["content"][0]["signature"],
        "signed-data"
    );
    assert_eq!(
        body["messages"][1]["content"][1],
        json!({"type": "redacted_thinking", "data": "opaque"})
    );
    assert_eq!(body["messages"][1]["content"][2]["name"], "Bash");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(
        body["messages"][2]["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 4096, "display": "summarized"})
    );
    req.model = "claude-opus-4-6".into();
    collect(provider.stream(req).await.unwrap()).await.unwrap();
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["thinking"]["display"], "summarized");
    assert_eq!(body["output_config"]["effort"], "medium");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
}

#[tokio::test]
async fn malformed_and_truncated_streams_fail() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let streams = [
        "data: not-json\n\n".to_owned(),
        sse_body(&text_events()[..4]),
        sse_body(&[
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Busy"}}),
        ]),
        sse_body(&[
            json!({"type": "message_start", "message": {"id": "msg"}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "c", "name": "t", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ]),
    ];
    for body in streams {
        h.server.reset().await;
        Mock::given(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&h.server)
            .await;
        let chunks: Vec<_> = provider.stream(request()).await.unwrap().collect().await;
        assert!(chunks.last().unwrap().is_err());
        assert_eq!(chunks.iter().filter(|c| c.is_err()).count(), 1);
        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, Ok(CompletionChunk::Done(_))))
        );
    }
}

#[tokio::test]
async fn lists_models_with_pagination() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("limit", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": [{"id": "z", "display_name": "Zed"}], "has_more": true, "last_id": "z"}),
        ))
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", "z"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            // Trimmed from a real claude-opus-5 entry.
            json!({"data": [{
                "id": "a", "display_name": "Alpha",
                "max_input_tokens": 1000000, "max_tokens": 128000,
                "capabilities": {
                    "effort": {"supported": true, "low": {"supported": true},
                        "medium": {"supported": true}, "high": {"supported": true},
                        "xhigh": {"supported": true}, "max": {"supported": true}},
                    "thinking": {"supported": true, "types": {
                        "enabled": {"supported": false}, "adaptive": {"supported": true},
                        "disabled": {"supported": true}}}
                }
            }], "has_more": false}),
        ))
        .with_priority(1)
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let models = provider.list_models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "a");
    assert_eq!(models[1].display_name, "Zed");
    let caps = &models[0].capabilities;
    assert_eq!(caps.context_window, Some(1_000_000));
    assert_eq!(caps.max_output_tokens, Some(128_000));
    assert_eq!(caps.adaptive_thinking, Some(true));
    assert_eq!(caps.budget_thinking, Some(false));
    assert_eq!(caps.thinking_off, Some(true));
    assert_eq!(
        caps.efforts.as_deref(),
        Some(
            &[
                loom_llm::ReasoningEffort::Low,
                loom_llm::ReasoningEffort::Medium,
                loom_llm::ReasoningEffort::High,
                loom_llm::ReasoningEffort::Xhigh,
            ][..]
        )
    );
    assert_eq!(
        models[1].capabilities,
        loom_llm::ModelCapabilities::default()
    );
}

#[tokio::test]
async fn retries_overload_and_surfaces_api_errors() {
    let mut h = harness().await;
    h.auth = h.auth.clone().with_retry(loom_llm::RetryPolicy {
        max_retries: 1,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_secs(1),
    });
    credential(&h, now() + 3600).await;
    Mock::given(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(529)
                .set_body_json(json!({"error": {"type": "overloaded_error", "message": "Busy"}})),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&h.server)
        .await;
    serve_stream(&h, &text_events()).await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    assert_eq!(
        collect(provider.stream(request()).await.unwrap())
            .await
            .unwrap()
            .text(),
        "Hello"
    );
    h.server.reset().await;
    Mock::given(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            json!({"error": {"type": "authentication_error", "message": "Bad token"}}),
        ))
        .expect(1)
        .mount(&h.server)
        .await;
    let error = match provider.stream(request()).await {
        Ok(_) => panic!(),
        Err(error) => error,
    };
    assert!(matches!(error, LlmError::Api { status: 401, .. }));
    assert_eq!(error.code(), Some("authentication_error"));
}

#[tokio::test]
async fn invalid_token_responses_are_not_saved() {
    let h = harness().await;
    credential(&h, 0).await;
    let before = h.section(ID).await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    for body in [
        "not-json".to_owned(),
        json!({"access_token": "", "refresh_token": "new", "expires_in": 3600}).to_string(),
        json!({"access_token": "access", "refresh_token": "new", "expires_in": 0}).to_string(),
    ] {
        h.server.reset().await;
        Mock::given(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&h.server)
            .await;
        let error = match provider.stream(request()).await {
            Ok(_) => panic!(),
            Err(error) => error,
        };
        assert!(matches!(error, LlmError::Protocol { .. }));
        assert_eq!(h.section(ID).await, before);
    }
}

#[tokio::test]
async fn usage_limits_come_from_the_oauth_usage_endpoint() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/usage"))
        .and(header("authorization", "Bearer sk-ant-oat-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour": {"utilization": 23.0, "resets_at": "2026-11-10T17:00:00Z"},
            "seven_day": {"utilization": 41.0, "resets_at": null},
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    // A base URL that names the API version still finds the usage endpoint.
    let provider = h.provider(ID, json!({"base_url": format!("{}/v1", h.server.uri())}));
    let limits = provider.usage_limits().await.unwrap().unwrap().limits;
    let requests = h.server.received_requests().await.unwrap();
    let beta = requests[0].headers["anthropic-beta"].to_str().unwrap();
    assert!(beta.split(',').any(|b| b == "oauth-2025-04-20"), "{beta}");
    let summary: Vec<_> = limits
        .iter()
        .map(|l| (l.label.as_str(), l.used_percent, l.resets_at, l.headline))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("5h", Some(23), Some(1_794_330_000), true),
            ("weekly", Some(41), None, true)
        ]
    );
}

#[tokio::test]
async fn usage_limit_errors_are_reported() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/api/oauth/usage"))
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"error": {"type": "permission_error", "message": "no usage for you"}}),
        ))
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let error = provider.usage_limits().await.unwrap_err();
    assert!(error.to_string().contains("no usage for you"), "{error}");
}

#[tokio::test]
async fn managed_settings_come_from_the_claude_code_settings_endpoint() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(method("GET"))
        .and(path("/api/claude_code/settings"))
        .and(header("authorization", "Bearer sk-ant-oat-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "uuid": "0b6c",
            "checksum": "abc",
            "settings": {"env": {"OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.example.com"}},
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": format!("{}/v1", h.server.uri())}));
    let settings = provider.managed_settings().await.unwrap().unwrap();
    assert_eq!(
        Value::Object(settings),
        json!({"env": {"OTEL_EXPORTER_OTLP_ENDPOINT": "https://otel.example.com"}})
    );
    let requests = h.server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers["anthropic-beta"], "oauth-2025-04-20");
}

#[tokio::test]
async fn organizations_without_managed_settings_have_none() {
    for status in [204, 404] {
        let h = harness().await;
        credential(&h, now() + 3600).await;
        Mock::given(path("/api/claude_code/settings"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&h.server)
            .await;
        let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
        let settings = provider.managed_settings().await.unwrap();
        assert_eq!(settings, Some(serde_json::Map::new()), "{status}");
    }
}

#[tokio::test]
async fn managed_settings_errors_are_reported() {
    let h = harness().await;
    credential(&h, now() + 3600).await;
    Mock::given(path("/api/claude_code/settings"))
        .respond_with(ResponseTemplate::new(403).set_body_json(
            json!({"error": {"type": "permission_error", "message": "not your organization"}}),
        ))
        .mount(&h.server)
        .await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let error = provider.managed_settings().await.unwrap_err();
    assert!(
        error.to_string().contains("not your organization"),
        "{error}"
    );

    let h = harness().await;
    let provider = h.provider(ID, json!({"base_url": h.server.uri()}));
    let error = provider.managed_settings().await.unwrap_err();
    assert!(
        matches!(error, LlmError::NotAuthenticated { .. }),
        "{error}"
    );
}
