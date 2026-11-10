mod common;

use std::collections::HashMap;

use common::{Harness, TEST_JWKS, codex_access_token, now, sign_id_token};
use loom_llm::auth::CallbackParams;
use loom_llm::{LlmError, LoginInput, LoginMethod, LoginStarted, ProviderId};
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

async fn start_siwc(h: &Harness) -> (String, HashMap<String, String>) {
    let started = h
        .auth
        .start_login(
            ProviderId::OpenAi,
            LoginMethod::Browser {
                redirect_uri: Some(REDIRECT.into()),
                account: None,
            },
        )
        .await
        .unwrap();
    let LoginStarted::Browser { login_id, url, .. } = started else {
        panic!("expected browser login");
    };
    let query = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    (login_id, query)
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

fn callback(query: &HashMap<String, String>, client_id: Option<&str>) -> LoginInput {
    LoginInput::Callback(CallbackParams {
        code: Some("auth-code".into()),
        state: Some(query["state"].clone()),
        client_id: client_id.map(str::to_owned),
        ..CallbackParams::default()
    })
}

#[tokio::test]
async fn chatgpt_login_registers_and_refreshes() {
    let h = Harness::new().await;
    mount_discovery(&h).await;
    let (login_id, query) = start_siwc(&h).await;
    assert_eq!(query["client_id"], "dynamic_agent_client");
    assert_eq!(query["redirect_uri"], REDIRECT);
    let host_id = query["ext_agent_host_id"].clone();
    assert!(host_id.starts_with("urn:uuid:"));

    mount_token(
        &h,
        "authorization_code",
        json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "id_token": id_token(&h, "oaiapp_1", &query["nonce"]),
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
        }),
    )
    .await;
    let info = h
        .auth
        .complete_login(&login_id, callback(&query, Some("oaiapp_1")))
        .await
        .unwrap();
    assert_eq!(info.email.as_deref(), Some("user@example.com"));
    assert_eq!(info.account_id.as_deref(), Some("oaiapp_1"));
    assert_eq!(h.auth.chatgpt_access_token().await.unwrap(), "access-1");

    let requests = h.server.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|r| r.url.path() == "/api/accounts/oauth/token")
        .unwrap();
    let form = String::from_utf8_lossy(&exchange.body);
    assert!(form.contains("client_id=oaiapp_1"));
    assert!(form.contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"));
    assert!(form.contains("code_verifier="));

    h.store
        .update(|c| {
            c.openai.accounts[0].tokens.as_mut().unwrap().expires_at = 0;
        })
        .await
        .unwrap();
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
    assert_eq!(h.auth.chatgpt_access_token().await.unwrap(), "access-2");
    let creds = h.store.load().await.unwrap();
    let account = creds.openai.active_account().unwrap();
    assert_eq!(account.tokens.as_ref().unwrap().refresh_token, "refresh-2");
    assert_eq!(creds.openai.host_id.as_deref(), Some(host_id.as_str()));

    let (_, reauth) = {
        let started = h
            .auth
            .start_login(
                ProviderId::OpenAi,
                LoginMethod::Browser {
                    redirect_uri: Some(REDIRECT.into()),
                    account: Some("oaiapp_1".into()),
                },
            )
            .await
            .unwrap();
        let LoginStarted::Browser { login_id, url, .. } = started else {
            panic!()
        };
        let q: HashMap<String, String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        (login_id, q)
    };
    assert_eq!(reauth["client_id"], "oaiapp_1");
    assert_eq!(reauth["login_hint"], "user@example.com");
    assert_eq!(reauth["ext_agent_host_id"], host_id);
}

#[tokio::test]
async fn chatgpt_login_rejects_bad_nonce_and_state() {
    let h = Harness::new().await;
    mount_discovery(&h).await;
    let (login_id, query) = start_siwc(&h).await;
    mount_token(
        &h,
        "authorization_code",
        json!({
            "access_token": "a",
            "refresh_token": "r",
            "id_token": id_token(&h, "oaiapp_1", "wrong-nonce"),
            "expires_in": 3600,
            "scope": "chatgpt.tokens.use.direct",
        }),
    )
    .await;
    let err = h
        .auth
        .complete_login(&login_id, callback(&query, Some("oaiapp_1")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nonce"), "{err}");
    assert!(h.store.load().await.unwrap().openai.accounts.is_empty());

    let (login_id, query) = start_siwc(&h).await;
    let mut bad = query.clone();
    bad.insert("state".into(), "other".into());
    let err = h
        .auth
        .complete_login(&login_id, callback(&bad, Some("oaiapp_1")))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("state"), "{err}");
}

#[tokio::test]
async fn chatgpt_login_requires_issued_client_id() {
    let h = Harness::new().await;
    let (login_id, query) = start_siwc(&h).await;
    let err = h
        .auth
        .complete_login(&login_id, callback(&query, None))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("client ID"), "{err}");
}

#[tokio::test]
async fn chatgpt_logout_revokes_and_keeps_registration() {
    let h = Harness::new().await;
    mount_discovery(&h).await;
    let (login_id, query) = start_siwc(&h).await;
    mount_token(
        &h,
        "authorization_code",
        json!({
            "access_token": "a",
            "refresh_token": "r",
            "id_token": id_token(&h, "oaiapp_1", &query["nonce"]),
            "expires_in": 3600,
            "scope": "chatgpt.tokens.use.direct",
        }),
    )
    .await;
    h.auth
        .complete_login(&login_id, callback(&query, Some("oaiapp_1")))
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .and(body_string_contains("token_type_hint=refresh_token"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.server)
        .await;
    h.auth.logout(ProviderId::OpenAi).await.unwrap();
    let status = h.auth.status().await.unwrap();
    assert_eq!(status.openai_accounts.len(), 1);
    assert!(!status.openai_accounts[0].signed_in);
    assert!(matches!(
        h.auth.chatgpt_access_token().await,
        Err(LlmError::NotAuthenticated { .. })
    ));
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
        .start_login(ProviderId::OpenAiCodex, LoginMethod::DeviceCode)
        .await
        .unwrap();
    let LoginStarted::DeviceCode {
        login_id,
        user_code,
        verification_uri,
        ..
    } = started
    else {
        panic!("expected device code");
    };
    assert_eq!(user_code, "ABCD-1234");
    assert!(verification_uri.ends_with("/codex/device"));
    let info = h
        .auth
        .wait_device_login(&login_id, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(info.account_id.as_deref(), Some("acct-9"));
    let (token, account) = h.auth.codex_access_token().await.unwrap();
    assert_eq!(account, "acct-9");
    assert_eq!(token, codex_access_token("acct-9"));
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
        .start_login(ProviderId::OpenAiCodex, LoginMethod::DeviceCode)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = h
        .auth
        .wait_device_login(started.login_id(), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Cancelled));
}

#[tokio::test]
async fn codex_browser_login_accepts_pasted_redirect_and_refreshes() {
    let h = Harness::new().await;
    let started = h
        .auth
        .start_login(
            ProviderId::OpenAiCodex,
            LoginMethod::Browser {
                redirect_uri: None,
                account: None,
            },
        )
        .await
        .unwrap();
    let LoginStarted::Browser {
        login_id,
        url,
        redirect_uri,
    } = started
    else {
        panic!()
    };
    assert_eq!(redirect_uri, "http://localhost:1455/auth/callback");
    let state = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
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
            LoginInput::PastedRedirect {
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
    let (_, account) = h.auth.codex_access_token().await.unwrap();
    assert_eq!(account, "acct-1");
    let creds = h.store.load().await.unwrap().openai_codex.unwrap();
    assert_eq!(creds.refresh_token, "r1");
    assert!(creds.expires_at > now() + 3000);
}
