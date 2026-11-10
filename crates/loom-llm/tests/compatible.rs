mod common;

use common::{Harness, id, sse_body};
use loom_config::{LlmConfig, ProvidersConfig};
use loom_llm::{LoginMethodInfo, Message, ProviderSet, collect, validate_config};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

fn config(entries: Vec<(&str, Option<Value>)>) -> LlmConfig {
    LlmConfig {
        default_model: None,
        providers: ProvidersConfig(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        ),
    }
}

#[tokio::test]
async fn custom_entries_stream_chat_completions() {
    let h = Harness::new().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer local-key"))
        .and(body_partial_json(json!({"model": "qwen", "stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(format!(
                    "{}data: [DONE]\n\n",
                    sse_body(&[
                        json!({"id": "x", "choices": [{"delta": {"content": "Hi "}}]}),
                        json!({"choices": [{"delta": {"content": "there"}, "finish_reason": "stop"}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}}),
                    ])
                )),
        )
        .mount(&h.server)
        .await;
    let settings = json!({
        "type": "openai-compatible",
        "base_url": format!("{}/v1", h.server.uri()),
        "display_name": "Local",
        "models": ["qwen", "llama"],
    });
    let llm = config(vec![("local", Some(settings))]);
    validate_config(&llm).unwrap();
    let set = ProviderSet::from_config(&llm, &h.auth).unwrap();
    set.activate(&h.auth);
    assert!(set.contains(&id("local")));

    let status = h.auth.provider_status(&id("local")).await.unwrap();
    assert_eq!(status.display_name, "Local");
    assert_eq!(status.login_methods, vec![LoginMethodInfo::ApiKey]);
    assert!(!status.api_key_set);
    h.auth
        .set_api_key(&id("local"), Some("local-key".into()))
        .await
        .unwrap();
    assert!(
        h.auth
            .provider_status(&id("local"))
            .await
            .unwrap()
            .api_key_set
    );
    assert!(h.auth.providers().contains(&id("local")));

    let provider = set.get(&id("local")).unwrap();
    let models = provider.list_models().await.unwrap();
    assert_eq!(models.len(), 2);
    let message = collect(
        provider
            .stream(loom_llm::CompletionRequest {
                model: "qwen".into(),
                messages: vec![Message::user_text("hello")],
                ..Default::default()
            })
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(message.text(), "Hi there");
    assert_eq!(message.usage.total, 6);

    h.auth.logout(&id("local")).await.unwrap();
    assert!(
        !h.auth
            .provider_status(&id("local"))
            .await
            .unwrap()
            .api_key_set
    );
    ProviderSet::from_config(&config(vec![]), &h.auth)
        .unwrap()
        .activate(&h.auth);
    assert!(h.auth.provider_status(&id("local")).await.is_err());
}

#[test]
fn rejects_unknown_and_malformed_entries() {
    for (entries, needle) in [
        (vec![("mystery", Some(json!({})))], "unknown provider"),
        (vec![("mystery", None)], "unknown provider"),
        (
            vec![("x", Some(json!({"type": "openai-compatible"})))],
            "base_url",
        ),
        (
            vec![(
                "x",
                Some(json!({"type": "openai-compatible", "base_url": "ftp://x"})),
            )],
            "https://",
        ),
        (
            vec![(
                "x",
                Some(json!({"type": "openai-compatible", "base_url": "http://x", "extra": 1})),
            )],
            "extra",
        ),
    ] {
        let err = validate_config(&config(entries)).unwrap_err().to_string();
        assert!(err.contains(needle), "{err}");
    }
    let mut llm = config(vec![(
        "x",
        Some(json!({"type": "openai-compatible", "base_url": "http://x"})),
    )]);
    llm.default_model = Some("x/model".into());
    validate_config(&llm).unwrap();
    llm.default_model = Some("y/model".into());
    assert!(validate_config(&llm).is_err());
}
