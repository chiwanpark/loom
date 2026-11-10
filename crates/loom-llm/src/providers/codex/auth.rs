use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{LlmError, Result};
use crate::oauth::callback::CallbackParams;
use crate::oauth::jwt::unverified_claims;
use crate::oauth::pkce::{Pkce, random_token};
use crate::oauth::token_error;
use crate::store::now_unix;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCredential {
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub saved_at: u64,
}

impl CodexCredential {
    pub fn is_fresh(&self, margin_secs: u64) -> bool {
        self.expires_at > now_unix() + margin_secs
    }
}

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const CALLBACK_PORT: u16 = 1455;
pub const SCOPE: &str = "openid profile email offline_access";
pub const ORIGINATOR: &str = "loom";
pub const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";
pub const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEndpoints {
    pub base: String,
}

impl Default for CodexEndpoints {
    fn default() -> Self {
        Self {
            base: "https://auth.openai.com".into(),
        }
    }
}

impl CodexEndpoints {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base.trim_end_matches('/'))
    }

    pub fn verification_uri(&self) -> String {
        self.url("/codex/device")
    }
}

#[derive(Debug, Clone)]
pub struct CodexPending {
    pub url: String,
    state: String,
    verifier: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuth {
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CodexClient {
    http: reqwest::Client,
    endpoints: CodexEndpoints,
}

pub fn account_id_from_token(access_token: &str) -> Option<String> {
    unverified_claims(access_token)?
        .get(ACCOUNT_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_manual_input(input: &str) -> CallbackParams {
    let value = input.trim();
    if let Ok(url) = Url::parse(value)
        && url.has_host()
    {
        return CallbackParams::from_query(url.query().unwrap_or_default());
    }
    if let Some((code, state)) = value.split_once('#') {
        return CallbackParams {
            code: Some(code.to_owned()),
            state: Some(state.to_owned()),
            ..CallbackParams::default()
        };
    }
    if value.contains("code=") {
        return CallbackParams::from_query(value);
    }
    CallbackParams {
        code: Some(value.to_owned()).filter(|v| !v.is_empty()),
        ..CallbackParams::default()
    }
}

impl CodexClient {
    pub fn with_endpoints(http: reqwest::Client, endpoints: CodexEndpoints) -> Self {
        Self { http, endpoints }
    }

    pub fn start_browser(&self) -> Result<CodexPending> {
        let pkce = Pkce::generate();
        let state = random_token();
        let mut url = Url::parse(&self.endpoints.url("/oauth/authorize"))
            .map_err(|e| LlmError::login(format!("invalid authorize URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", ORIGINATOR);
        Ok(CodexPending {
            url: url.into(),
            state,
            verifier: pkce.verifier,
        })
    }

    pub async fn complete_browser(
        &self,
        pending: &CodexPending,
        callback: &CallbackParams,
    ) -> Result<CodexCredential> {
        callback.check_error()?;
        if callback.state.is_some() {
            callback.check_state(&pending.state)?;
        }
        let code = callback.require_code()?;
        self.exchange(code, &pending.verifier, REDIRECT_URI).await
    }

    pub async fn complete_manual(
        &self,
        pending: &CodexPending,
        input: &str,
    ) -> Result<CodexCredential> {
        self.complete_browser(pending, &parse_manual_input(input))
            .await
    }

    pub async fn start_device(&self) -> Result<DeviceAuth> {
        let response = self
            .http
            .post(self.endpoints.url("/api/accounts/deviceauth/usercode"))
            .json(&json!({ "client_id": CLIENT_ID }))
            .send()
            .await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(LlmError::login(
                "device code login is not enabled for this server; use browser login",
            ));
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(LlmError::login(format!(
                "device code request failed ({status}): {body}"
            )));
        }
        let body: Value = response.json().await?;
        let interval = match &body["interval"] {
            Value::String(s) => s.trim().parse::<f64>().ok(),
            v => v.as_f64(),
        }
        .filter(|v| v.is_finite() && *v >= 0.0);
        match (
            body["device_auth_id"].as_str(),
            body["user_code"].as_str(),
            interval,
        ) {
            (Some(id), Some(code), Some(interval)) => Ok(DeviceAuth {
                device_auth_id: id.to_owned(),
                user_code: code.to_owned(),
                verification_uri: self.endpoints.verification_uri(),
                interval: Duration::from_secs_f64(interval),
            }),
            _ => Err(LlmError::login(format!(
                "invalid device code response: {body}"
            ))),
        }
    }

    pub async fn poll_device(
        &self,
        device: &DeviceAuth,
        cancel: &CancellationToken,
    ) -> Result<CodexCredential> {
        let deadline = tokio::time::Instant::now() + DEVICE_TIMEOUT;
        let mut interval = device.interval;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                _ = tokio::time::sleep(interval) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(LlmError::login(
                    "device code expired; start the login again",
                ));
            }
            let response = self
                .http
                .post(self.endpoints.url("/api/accounts/deviceauth/token"))
                .json(&json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                }))
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
                let body: Value = response.json().await?;
                let (Some(code), Some(verifier)) = (
                    body["authorization_code"].as_str(),
                    body["code_verifier"].as_str(),
                ) else {
                    return Err(LlmError::login(format!(
                        "invalid device token response: {body}"
                    )));
                };
                let redirect = self.endpoints.url("/deviceauth/callback");
                return self.exchange(code, verifier, &redirect).await;
            }
            if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND
            {
                continue;
            }
            let body = response.text().await.unwrap_or_default();
            let parsed: Option<Value> = serde_json::from_str(&body).ok();
            let code = parsed.as_ref().and_then(|v| {
                v["error"]["code"]
                    .as_str()
                    .or_else(|| v["error"].as_str())
                    .map(str::to_owned)
            });
            match code.as_deref() {
                Some("deviceauth_authorization_pending" | "authorization_pending") => continue,
                Some("slow_down") => interval += Duration::from_secs(5),
                _ => {
                    return Err(LlmError::login(format!(
                        "device login failed ({status}): {body}"
                    )));
                }
            }
        }
    }

    pub async fn refresh(&self, credential: &CodexCredential) -> Result<CodexCredential> {
        let token = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &credential.refresh_token),
                ("client_id", CLIENT_ID),
            ])
            .await?;
        credential_from_response(token, Some(credential))
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<CodexCredential> {
        let token = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ])
            .await?;
        credential_from_response(token, None)
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let response = self
            .http
            .post(self.endpoints.url("/oauth/token"))
            .form(form)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(token_error("openai-codex", status.as_u16(), &body));
        }
        Ok(response.json().await?)
    }
}

fn credential_from_response(
    token: TokenResponse,
    previous: Option<&CodexCredential>,
) -> Result<CodexCredential> {
    let access_token = token
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| LlmError::login("token response has no access_token"))?;
    let refresh_token = token
        .refresh_token
        .filter(|t| !t.is_empty())
        .or_else(|| previous.map(|p| p.refresh_token.clone()))
        .ok_or_else(|| LlmError::login("token response has no refresh_token"))?;
    let expires_in = token
        .expires_in
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| LlmError::login("token response has an invalid expires_in"))?;
    let account_id = account_id_from_token(&access_token)
        .or_else(|| previous.map(|p| p.account_id.clone()))
        .ok_or_else(|| LlmError::login("access token has no ChatGPT account ID"))?;
    let now = now_unix();
    Ok(CodexCredential {
        account_id,
        access_token,
        refresh_token,
        expires_at: now + expires_in as u64,
        saved_at: now,
    })
}

#[cfg(test)]
pub(crate) fn fake_access_token(account_id: &str) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let payload = json!({ ACCOUNT_CLAIM: { "chatgpt_account_id": account_id } });
    format!(
        "h.{}.s",
        URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_account_id() {
        assert_eq!(
            account_id_from_token(&fake_access_token("acc-1")).as_deref(),
            Some("acc-1")
        );
        assert_eq!(account_id_from_token("h.e30.s"), None);
    }

    #[test]
    fn browser_url_has_codex_parameters() {
        let client = CodexClient::with_endpoints(reqwest::Client::new(), CodexEndpoints::default());
        let pending = client.start_browser().unwrap();
        let url = Url::parse(&pending.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/oauth/authorize");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["originator"], "loom");
        assert_eq!(q["codex_cli_simplified_flow"], "true");
        assert_eq!(q["id_token_add_organizations"], "true");
        assert_eq!(q["state"], pending.state);
    }

    #[test]
    fn parses_manual_inputs() {
        let p = parse_manual_input("http://localhost:1455/auth/callback?code=c1&state=s1");
        assert_eq!(
            (p.code.as_deref(), p.state.as_deref()),
            (Some("c1"), Some("s1"))
        );
        let p = parse_manual_input("c2#s2");
        assert_eq!(
            (p.code.as_deref(), p.state.as_deref()),
            (Some("c2"), Some("s2"))
        );
        let p = parse_manual_input("code=c3&state=s3");
        assert_eq!(
            (p.code.as_deref(), p.state.as_deref()),
            (Some("c3"), Some("s3"))
        );
        let p = parse_manual_input("  c4 ");
        assert_eq!((p.code.as_deref(), p.state), (Some("c4"), None));
    }
}
