mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{Harness, id, now, sse_body};
use loom_llm::{
    Auth, CompletionRequest, CredentialLock, CredentialSource, CredentialStore, Credentials,
    HttpClient, LlmError, LoginMethod, Message, Result, collect,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

const ID: &str = "anthropic";

struct Lent {
    credentials: Mutex<Credentials>,
}

struct NoSave(Credentials);

#[async_trait]
impl CredentialLock for NoSave {
    async fn load(&self) -> Result<Credentials> {
        Ok(self.0.clone())
    }

    async fn save(&self, _credentials: Credentials) -> Result<()> {
        Err(LlmError::Unsupported("read only".into()))
    }
}

#[async_trait]
impl CredentialSource for Lent {
    fn location(&self) -> PathBuf {
        PathBuf::from("broker")
    }

    async fn load(&self) -> Result<Credentials> {
        Ok(self.credentials.lock().unwrap().clone())
    }

    async fn lock(&self) -> Result<Box<dyn CredentialLock>> {
        Ok(Box::new(NoSave(self.credentials.lock().unwrap().clone())))
    }

    fn read_only(&self) -> Option<String> {
        Some("the broker keeps these accounts; sign in there".into())
    }

    async fn account_names(&self) -> BTreeMap<String, String> {
        [(ID.to_owned(), "work".to_owned())].into_iter().collect()
    }
}

#[tokio::test]
async fn a_lent_credential_is_fresh_and_has_no_refresh_token() {
    let h = Harness::new().await;
    let auth = h.auth.clone().with_auth_base_url(ID, h.server.uri());
    h.set_section(
        ID,
        json!({
            "access_token": "old-access", "refresh_token": "refresh-old", "expires_at": now() + 600,
            "account_id": "account-1", "label": "me@example.com",
        }),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .and(body_json(json!({"grant_type": "refresh_token", "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e", "refresh_token": "refresh-old"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "new-access", "refresh_token": "refresh-new", "expires_in": 3600})))
        .expect(1)
        .mount(&h.server)
        .await;

    let lent = auth.lend(&id(ID), 300).await.unwrap().unwrap();
    assert_eq!(lent.section["access_token"], "old-access");
    assert_eq!(lent.section["refresh_token"], "");
    assert_eq!(
        lent.expires_at,
        Some(lent.section["expires_at"].as_u64().unwrap())
    );

    let lent = auth.lend(&id(ID), 900).await.unwrap().unwrap();
    assert_eq!(lent.section["access_token"], "new-access");
    assert_eq!(lent.section["refresh_token"], "");
    assert_eq!(lent.section["label"], "me@example.com");
    assert!(!format!("{lent:?}").contains("new-access"));
    assert_eq!(h.section(ID).await["refresh_token"], "refresh-new");

    assert!(auth.lend(&id("openai-codex"), 900).await.unwrap().is_none());
}

#[tokio::test]
async fn an_agent_uses_lent_credentials_and_cannot_sign_in() {
    let h = Harness::new().await;
    let mut credentials = Credentials::default();
    credentials.set_section(
        ID,
        Some(json!({
            "access_token": "lent-access", "refresh_token": "", "expires_at": now() + 3600,
            "account_id": "account-1", "label": "me@example.com",
        })),
    );
    let source = Arc::new(Lent {
        credentials: Mutex::new(credentials),
    });
    let auth = Auth::new(CredentialStore::from_source(source), HttpClient::new());
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer lent-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(&[
                    json!({"type": "message_start", "message": {"id": "m", "model": "x", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
                    json!({"type": "content_block_stop", "index": 0}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
                    json!({"type": "message_stop"}),
                ])),
        )
        .mount(&h.server)
        .await;
    let config = loom_config::LlmConfig {
        providers: loom_config::ProvidersConfig(
            [(ID.to_owned(), Some(json!({"base_url": h.server.uri()})))]
                .into_iter()
                .collect(),
        ),
        ..loom_config::LlmConfig::default()
    };
    let provider = loom_llm::ProviderSet::from_config(&config, &auth)
        .unwrap()
        .get(&id(ID))
        .unwrap();
    let request = CompletionRequest {
        model: "claude".into(),
        messages: vec![Message::user_text("Hello")],
        ..Default::default()
    };
    let answer = collect(provider.stream(request).await.unwrap())
        .await
        .unwrap();
    assert_eq!(answer.text(), "Hi");

    let refused = auth
        .start_login(
            &id(ID),
            LoginMethod::Browser {
                redirect_uri: None,
                account: None,
            },
        )
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("sign in there"), "{refused}");
    assert!(auth.logout(&id(ID)).await.is_err());
    assert!(
        auth.set_api_key(&id("openai"), Some("k".into()))
            .await
            .is_err()
    );

    let status = auth.provider_status(&id(ID)).await.unwrap();
    assert!(status.login_methods.is_empty());
    assert_eq!(
        status.accounts[0].label.as_deref(),
        Some("work (me@example.com)")
    );
    assert!(status.signed_in());
}
