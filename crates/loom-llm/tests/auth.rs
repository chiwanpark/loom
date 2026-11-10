mod common;

use std::collections::HashMap;

use common::{CODEX, Harness, OPENAI, TEST_JWKS, codex_access_token, id, now, sign_id_token};
use loom_llm::{LlmError, LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, ResponseTemplate};

const REDIRECT: &str = "http://127.0.0.1:5555/auth/callback";

async fn mount_discovery(h: &Harness) {
    let jwks: Value = serde_json::from_str(TEST_JWKS).unwrap();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": h.server.uri(),
            "jwks_uri": format!("{}/jwks", h.server.uri()),
            "revocation_endpoint": format!("{}/oauth/revoke", h.server.uri()),
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks))
        .mount(&h.server)
        .await;
}

fn query(url: &str) -> HashMap<String, String> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

async fn start_browser(h: &Harness, provider: &str, account: Option<&str>) -> (String, String) {
    let started = h
        .auth
        .start_login(
            &id(provider),
            LoginMethod::Browser {
                redirect_uri: Some(REDIRECT.into()),
                account: account.map(str::to_owned),
            },
        )
        .await
        .unwrap();
    let LoginPrompt::OpenUrl { url, .. } = started.prompt else {
        panic!("expected a browser prompt");
    };
    (started.login_id, url)
}

fn id_token(h: &Harness, client_id: &str, nonce: &str) -> String {
    sign_id_token(&json!({
        "iss": h.server.uri(),
        "aud": client_id,
        "sub": "user-1",
        "email": "user@example.com",
        "nonce": nonce,
        "exp": now() + 3600,
        "iat": now(),
    }))
}

async fn mount_token(h: &Harness, grant: &str, body: Value) {
    Mock::given(method("POST"))
        .and(path("/api/accounts/oauth/token"))
        .and(body_string_contains(format!("grant_type={grant}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&h.server)
        .await;
}

fn redirect(state: &str, client_id: Option<&str>) -> LoginInput {
    let mut url = format!("{REDIRECT}?code=auth-code&state={state}");
    if let Some(client_id) = client_id {
        url.push_str(&format!("&client_id={client_id}"));
    }
    LoginInput::Redirect { url }
}

fn grant(h: &Harness, nonce: &str, access: &str) -> Value {
    json!({
        "access_token": access,
        "refresh_token": "refresh-1",
        "id_token": id_token(h, "oaiapp_1", nonce),
        "token_type": "Bearer",
        "expires_in": 3600,
        "scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
    })
}

#[tokio::test]
async fn status_describes_login_methods() {
    let h = Harness::new().await;
    let status = h.auth.status().await.unwrap();
    let openai = status.iter().find(|s| s.provider == OPENAI).unwrap();
    assert!(openai.login_methods.contains(&LoginMethodInfo::ApiKey));
    assert!(openai.login_methods.iter().any(|m| matches!(
        m,
        LoginMethodInfo::Browser { redirect } if redirect.port.is_none() && redirect.host == "127.0.0.1"
    )));
    let codex = status.iter().find(|s| s.provider == CODEX).unwrap();
    assert!(codex.login_methods.contains(&LoginMethodInfo::DeviceCode));
    assert!(codex.login_methods.iter().any(|m| matches!(
        m,
        LoginMethodInfo::Browser { redirect } if redirect.uri(1455) == "http://localhost:1455/auth/callback"
    )));
    assert!(status.iter().all(|s| !s.signed_in()));
}

#[tokio::test]
async fn chatgpt_login_registers_refreshes_and_reauthorizes() {
    let h = Harness::new().await;
    mount_discovery(&h).await;
    let (login_id, url) = start_browser(&h, OPENAI, None).await;
    let q = query(&url);
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["redirect_uri"], REDIRECT);
    let host_id = q["ext_agent_host_id"].clone();
    assert!(host_id.starts_with("urn:uuid:"));

    mount_token(&h, "authorization_code", grant(&h, &q["nonce"], "access-1")).await;
    let info = h
        .auth
        .complete_login(&login_id, redirect(&q["state"], Some("oaiapp_1")))
        .await
        .unwrap();
    assert_eq!(info.label.as_deref(), Some("user@example.com"));
    assert_eq!(info.account_id.as_deref(), Some("oaiapp_1"));
    let status = h.auth.provider_status(&id(OPENAI)).await.unwrap();
    assert!(status.signed_in());
    assert_eq!(status.accounts[0].id, "oaiapp_1");

    let requests = h.server.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|r| r.url.path() == "/api/accounts/oauth/token")
        .unwrap();
    let form = String::from_utf8_lossy(&exchange.body);
    assert!(form.contains("client_id=oaiapp_1"));
    assert!(form.contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"));
    assert!(form.contains("code_verifier="));

    let mut section = h.section(OPENAI).await;
    section["accounts"][0]["tokens"]["expires_at"] = json!(0);
    h.set_section(OPENAI, section).await;
    mount_token(
        &h,
        "refresh_token",
        json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "expires_in": 3600,
            "scope": "chatgpt.tokens.use.direct openid",
        }),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer access-2",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(common::sse_body(&common::text_response_events("ok"))),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    let provider = h.provider(
        OPENAI,
        json!({"base_url": format!("{}/v1", h.server.uri())}),
    );
    let message = loom_llm::collect(
        provider
            .stream(loom_llm::CompletionRequest {
                model: "m".into(),
                messages: vec![loom_llm::Message::user_text("hi")],
                ..Default::default()
            })
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(message.text(), "ok");
    let section = h.section(OPENAI).await;
    assert_eq!(
        section["accounts"][0]["tokens"]["refresh_token"],
        "refresh-2"
    );
    assert_eq!(section["host_id"], host_id.as_str());

    let (_, url) = start_browser(&h, OPENAI, Some("oaiapp_1")).await;
    let q = query(&url);
    assert_eq!(q["client_id"], "oaiapp_1");
    assert_eq!(q["login_hint"], "user@example.com");
    assert_eq!(q["ext_agent_host_id"], host_id);
}

#[tokio::test]
async fn chatgpt_login_rejects_bad_nonce_state_and_missing_client() {
    let h = Harness::new().await;
    mount_discovery(&h).await;
    let (login_id, url) = start_browser(&h, OPENAI, None).await;
    let q = query(&url);
    mount_token(&h, "authorization_code", grant(&h, "wrong-nonce", "a")).await;
    let err = h
        .auth
        .complete_login(&login_id, redirect(&q["state"], Some("oaiapp_1")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nonce"), "{err}");
    assert_eq!(h.section(OPENAI).await["accounts"], json!([]));

    let (login_id, _) = start_browser(&h, OPENAI, None).await;
    let err = h
        .auth
        .complete_login(&login_id, redirect("other", Some("oaiapp_1")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("state"), "{err}");

    let (login_id, url) = start_browser(&h, OPENAI, None).await;
    let err = h
        .auth
        .complete_login(&login_id, redirect(&query(&url)["state"], None))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("client ID"), "{err}");

    let err = h
        .auth
        .start_login(&id(OPENAI), LoginMethod::DeviceCode)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Unsupported(_)));
}

#[tokio::test]
async fn chatgpt_logout_revokes_and_keeps_registration() {
    let h = Harness::new().await;
    mount_discovery(&h).await;
    let (login_id, url) = start_browser(&h, OPENAI, None).await;
    let q = query(&url);
    mount_token(&h, "authorization_code", grant(&h, &q["nonce"], "a")).await;
    h.auth
        .complete_login(&login_id, redirect(&q["state"], Some("oaiapp_1")))
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .and(body_string_contains("token_type_hint=refresh_token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.server)
        .await;
    h.auth.logout(&id(OPENAI)).await.unwrap();
    let status = h.auth.provider_status(&id(OPENAI)).await.unwrap();
    assert_eq!(status.accounts.len(), 1);
    assert!(!status.signed_in());
    assert!(h.auth.select_account(&id(OPENAI), "oaiapp_1").await.is_ok());
    assert!(h.auth.select_account(&id(OPENAI), "missing").await.is_err());
}

#[tokio::test]
async fn api_keys_are_stored_per_provider() {
    let h = Harness::new().await;
    h.auth
        .set_api_key(&id(OPENAI), Some("sk-test".into()))
        .await
        .unwrap();
    assert!(
        h.auth
            .provider_status(&id(OPENAI))
            .await
            .unwrap()
            .signed_in()
    );
    assert!(matches!(
        h.auth.set_api_key(&id(CODEX), Some("x".into())).await,
        Err(LlmError::Unsupported(_))
    ));
    h.auth.set_api_key(&id(OPENAI), None).await.unwrap();
    assert!(
        !h.auth
            .provider_status(&id(OPENAI))
            .await
            .unwrap()
            .signed_in()
    );
}

#[tokio::test]
async fn codex_device_login_polls_until_approved() {
    let h = Harness::new().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "dev-1",
            "user_code": "ABCD-1234",
            "interval": "0",
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "authorization_code": "dev-code",
            "code_verifier": "dev-verifier",
        })))
        .with_priority(2)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("code=dev-code"))
        .and(body_string_contains("deviceauth%2Fcallback"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": codex_access_token("acct-9"),
            "refresh_token": "codex-refresh",
            "expires_in": 3600,
        })))
        .mount(&h.server)
        .await;

    let started = h
        .auth
        .start_login(&id(CODEX), LoginMethod::DeviceCode)
        .await
        .unwrap();
    let LoginPrompt::DeviceCode {
        user_code,
        verification_uri,
        ..
    } = &started.prompt
    else {
        panic!("expected a device code");
    };
    assert_eq!(user_code, "ABCD-1234");
    assert!(verification_uri.ends_with("/codex/device"));
    assert_eq!(h.auth.login_provider(&started.login_id), Some(id(CODEX)));
    let info = h
        .auth
        .wait_login(&started.login_id, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.account_id.as_deref(), Some("acct-9"));
    let status = h.auth.provider_status(&id(CODEX)).await.unwrap();
    assert!(status.signed_in());
    assert_eq!(status.accounts[0].id, "acct-9");
}

#[tokio::test]
async fn codex_device_login_can_be_cancelled() {
    let h = Harness::new().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_auth_id": "dev-1",
            "user_code": "X",
            "interval": 60,
        })))
        .mount(&h.server)
        .await;
    let started = h
        .auth
        .start_login(&id(CODEX), LoginMethod::DeviceCode)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = h
        .auth
        .wait_login(&started.login_id, &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Cancelled));
}

#[tokio::test]
async fn codex_browser_login_accepts_redirect_and_refreshes() {
    let h = Harness::new().await;
    let (login_id, url) = start_browser(&h, CODEX, None).await;
    let state = query(&url)["state"].clone();
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": codex_access_token("acct-1"),
            "refresh_token": "r1",
            "expires_in": 1,
        })))
        .mount(&h.server)
        .await;
    h.auth
        .complete_login(
            &login_id,
            LoginInput::Redirect {
                url: format!("http://localhost:1455/auth/callback?code=c&state={state}"),
            },
        )
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=r1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": codex_access_token("acct-1"),
            "expires_in": 3600,
        })))
        .expect(1)
        .mount(&h.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(common::sse_body(&common::text_response_events("ok"))),
        )
        .mount(&h.server)
        .await;
    let provider = h.provider(
        CODEX,
        json!({"base_url": format!("{}/backend-api", h.server.uri())}),
    );
    let stream = provider
        .stream(loom_llm::CompletionRequest {
            model: "m".into(),
            messages: vec![loom_llm::Message::user_text("hi")],
            ..Default::default()
        })
        .await
        .unwrap();
    loom_llm::collect(stream).await.unwrap();
    let section = h.section(CODEX).await;
    assert_eq!(section["refresh_token"], "r1");
    assert!(section["expires_at"].as_u64().unwrap() > now() + 3000);
}
