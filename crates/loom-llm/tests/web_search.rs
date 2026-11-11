mod common;

use common::{CODEX, Harness, codex_access_token, now, sse_body};
use loom_llm::{Freshness, LlmError, WebSearch, WebSearchQuery, WebSearchSettings, WebSource};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

const SEARCH_PATH: &str = "/backend-api/codex/responses";

fn search(h: &Harness) -> WebSearch {
    WebSearch::new(
        &h.auth,
        WebSearchSettings {
            model: "search-model".into(),
            endpoint: Some(format!("{}{SEARCH_PATH}", h.server.uri())),
            reasoning: None,
        },
    )
}

fn query(max_sources: usize) -> WebSearchQuery {
    WebSearchQuery {
        query: "latest tokio release".into(),
        max_sources,
        freshness: Freshness::Live,
    }
}

async fn sign_in(h: &Harness, token: &str) {
    h.set_section(
        CODEX,
        json!({
            "account_id": "acct-1",
            "access_token": token,
            "refresh_token": "r",
            "expires_at": now() + 3600,
            "saved_at": now(),
        }),
    )
    .await;
}

/// A stream that answers with `text` in two deltas.
fn answer(text: &str) -> ResponseTemplate {
    let (a, b) = text.split_at(text.len() / 2);
    let events = [
        json!({"type": "response.created", "response": {}}),
        json!({"type": "response.output_text.delta", "delta": a}),
        json!({"type": "response.output_text.delta", "delta": b}),
        json!({"type": "response.completed", "response": {}}),
    ];
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse_body(&events))
}

#[tokio::test]
async fn searches_with_the_codex_credentials() {
    let h = Harness::new().await;
    let search = search(&h);
    assert!(!search.signed_in().await);

    let token = codex_access_token("acct-1");
    sign_in(&h, &token).await;
    assert!(search.signed_in().await);
    let sources: Vec<_> = (1..=3)
        .map(|i| json!({"title": format!("T{i}"), "url": format!("https://e/{i}"), "snippet": "s"}))
        .collect();
    let text = json!({"summary": "Tokio 1.50 is out.", "sources": sources}).to_string();
    Mock::given(method("POST"))
        .and(path(SEARCH_PATH))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(header("chatgpt-account-id", "acct-1"))
        .and(header("originator", "loom"))
        .and(body_partial_json(json!({
            "model": "search-model",
            "tools": [{"type": "web_search"}],
            "store": false,
            "stream": true,
        })))
        .respond_with(answer(&text))
        .expect(1)
        .mount(&h.server)
        .await;

    let result = search.search(&query(2)).await.unwrap();
    assert_eq!(result.summary, "Tokio 1.50 is out.");
    // The model returned three sources; only as many as asked for are kept.
    assert_eq!(
        result.sources,
        [1, 2]
            .map(|i| WebSource {
                title: format!("T{i}"),
                url: format!("https://e/{i}"),
                snippet: "s".into(),
            })
            .to_vec()
    );
}

#[tokio::test]
async fn needs_the_codex_credentials() {
    let h = Harness::new().await;
    let err = search(&h).search(&query(5)).await.unwrap_err();
    assert!(
        matches!(&err, LlmError::NotAuthenticated { provider, .. } if provider == CODEX),
        "{err}"
    );
    // Nothing was sent without them.
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn rejected_credentials_ask_to_sign_in_again() {
    let h = Harness::new().await;
    sign_in(&h, &codex_access_token("acct-1")).await;
    Mock::given(method("POST"))
        .and(path(SEARCH_PATH))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
        .mount(&h.server)
        .await;
    let err = search(&h).search(&query(5)).await.unwrap_err();
    assert!(matches!(&err, LlmError::NotAuthenticated { .. }), "{err}");
    assert!(err.to_string().contains("sign in to OpenAI Codex again"));
}

#[tokio::test]
async fn other_errors_carry_the_status_and_message() {
    let h = Harness::new().await;
    sign_in(&h, &codex_access_token("acct-1")).await;
    Mock::given(method("POST"))
        .and(path(SEARCH_PATH))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": {"message": "unknown model"}})),
        )
        .mount(&h.server)
        .await;
    let err = search(&h).search(&query(5)).await.unwrap_err();
    assert!(
        matches!(&err, LlmError::Api { status: 400, message, .. } if message == "unknown model"),
        "{err}"
    );
}
