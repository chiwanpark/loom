#![allow(dead_code)]

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use loom_config::{LlmConfig, ProvidersConfig};
use loom_llm::{Auth, CredentialStore, HttpClient, LlmProvider, ProviderId, ProviderSet};
use serde_json::{Value, json};
use wiremock::MockServer;

pub const TEST_KEY: &[u8] = include_bytes!("../fixtures/test_rsa.pem");
pub const TEST_JWKS: &str = include_str!("../fixtures/test_jwks.json");
pub const OPENAI: &str = "openai";
pub const CODEX: &str = "openai-codex";
const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";

pub struct Harness {
    pub server: MockServer,
    pub auth: Auth,
    pub store: CredentialStore,
    _dir: tempfile::TempDir,
}

impl Harness {
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        let auth = Auth::new(store.clone(), HttpClient::new())
            .with_auth_base_url(OPENAI, server.uri())
            .with_auth_base_url(CODEX, server.uri());
        Self {
            server,
            auth,
            store,
            _dir: dir,
        }
    }

    pub fn provider(&self, id: &str, settings: Value) -> Arc<dyn LlmProvider> {
        let config = LlmConfig {
            default_model: None,
            providers: ProvidersConfig([(id.to_owned(), Some(settings))].into_iter().collect()),
        };
        ProviderSet::from_config(&config, &self.auth)
            .unwrap()
            .get(&ProviderId::new(id))
            .unwrap()
    }

    pub async fn set_section(&self, id: &str, value: Value) {
        let id = id.to_owned();
        self.store
            .update(move |c| c.set_section(&id, Some(value)))
            .await
            .unwrap();
    }

    pub async fn section(&self, id: &str) -> Value {
        self.store
            .load()
            .await
            .unwrap()
            .section(id)
            .cloned()
            .unwrap_or(Value::Null)
    }
}

pub fn id(value: &str) -> ProviderId {
    ProviderId::new(value)
}

pub fn sign_id_token(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("test-key".into());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(TEST_KEY).unwrap(),
    )
    .unwrap()
}

pub fn codex_access_token(account_id: &str) -> String {
    let payload = json!({ ACCOUNT_CLAIM: { "chatgpt_account_id": account_id } });
    format!(
        "h.{}.s",
        URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
    )
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| {
            format!(
                "event: {}\ndata: {}\n\n",
                e["type"].as_str().unwrap_or(""),
                e
            )
        })
        .collect()
}

pub fn text_response_events(text: &str) -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": {"id": "resp_1"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "msg_1"}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": text}]}}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}}}),
    ]
}
