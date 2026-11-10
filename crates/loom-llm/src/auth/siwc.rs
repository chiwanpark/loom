use serde::Deserialize;
use url::Url;

use super::callback::CallbackParams;
use super::jwt::{IdTokenCheck, verify_id_token};
use super::pkce::{Pkce, random_token};
use super::store::{ChatGptAccount, OAuthTokens, now_unix};
use crate::error::{LlmError, Result};

pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
pub const AGENT_NAME: &str = "Loom";
pub const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
pub const RESOURCE: &str = "https://api.openai.com/v1";
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const DEFAULT_PORT: u16 = 1455;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiwcEndpoints {
    pub issuer: String,
    pub authorize_url: String,
    pub token_url: String,
    pub discovery_url: String,
}

impl Default for SiwcEndpoints {
    fn default() -> Self {
        Self::for_issuer("https://auth.openai.com")
    }
}

impl SiwcEndpoints {
    pub fn for_issuer(issuer: &str) -> Self {
        let base = issuer.trim_end_matches('/');
        Self {
            issuer: issuer.to_owned(),
            authorize_url: format!("{base}/api/accounts/authorize"),
            token_url: format!("{base}/api/accounts/oauth/token"),
            discovery_url: format!("{base}/.well-known/openid-configuration"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SiwcPending {
    pub url: String,
    pub redirect_uri: String,
    state: String,
    nonce: String,
    verifier: String,
    client_id: Option<String>,
    subject: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Discovery {
    jwks_uri: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<f64>,
    scope: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SiwcClient {
    http: reqwest::Client,
    endpoints: SiwcEndpoints,
    agent_name: String,
}

pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

fn check_redirect_uri(redirect_uri: &str) -> Result<()> {
    let url = Url::parse(redirect_uri)
        .map_err(|e| LlmError::login(format!("invalid redirect URI: {e}")))?;
    let valid = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port().is_some()
        && url.path() == CALLBACK_PATH
        && url.query().is_none();
    if !valid {
        return Err(LlmError::login(format!(
            "redirect URI must look like http://127.0.0.1:<port>{CALLBACK_PATH}"
        )));
    }
    Ok(())
}

impl SiwcClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self::with_endpoints(http, SiwcEndpoints::default())
    }

    pub fn with_endpoints(http: reqwest::Client, endpoints: SiwcEndpoints) -> Self {
        Self {
            http,
            endpoints,
            agent_name: AGENT_NAME.to_owned(),
        }
    }

    pub fn start(
        &self,
        redirect_uri: &str,
        host_id: &str,
        account: Option<&ChatGptAccount>,
    ) -> Result<SiwcPending> {
        check_redirect_uri(redirect_uri)?;
        let pkce = Pkce::generate();
        let state = random_token();
        let nonce = random_token();
        let mut url = Url::parse(&self.endpoints.authorize_url)
            .map_err(|e| LlmError::login(format!("invalid authorize URL: {e}")))?;
        {
            let mut query = url.query_pairs_mut();
            match account {
                Some(account) => {
                    query.append_pair("client_id", &account.client_id);
                    if let Some(id_token) = &account.id_token {
                        query.append_pair("id_token_hint", id_token);
                    }
                    if let Some(email) = &account.email {
                        query.append_pair("login_hint", email);
                    }
                }
                None => {
                    query.append_pair("client_id", DYNAMIC_CLIENT_ID);
                    query.append_pair("agent_name_hint", &self.agent_name);
                }
            }
            query
                .append_pair("ext_agent_host_id", host_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("resource", RESOURCE)
                .append_pair("scope", SCOPE)
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256");
        }
        Ok(SiwcPending {
            url: url.into(),
            redirect_uri: redirect_uri.to_owned(),
            state,
            nonce,
            verifier: pkce.verifier,
            client_id: account.map(|a| a.client_id.clone()),
            subject: account.map(|a| a.subject.clone()),
        })
    }

    pub async fn complete(
        &self,
        pending: &SiwcPending,
        callback: &CallbackParams,
    ) -> Result<ChatGptAccount> {
        callback.check_state(&pending.state)?;
        callback.check_error()?;
        let code = callback.require_code()?;
        let client_id = match (&pending.client_id, &callback.client_id) {
            (Some(expected), Some(got)) if expected != got => {
                return Err(LlmError::login(
                    "callback returned a different client ID than the selected account",
                ));
            }
            (Some(expected), _) => expected.clone(),
            (None, Some(got)) => got.clone(),
            (None, None) => {
                return Err(LlmError::login(
                    "registration callback did not contain an issued client ID",
                ));
            }
        };
        if client_id == DYNAMIC_CLIENT_ID {
            return Err(LlmError::login(
                "callback returned the registration client ID instead of an issued one",
            ));
        }
        let token = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client_id),
                ("code", code),
                ("code_verifier", &pending.verifier),
                ("redirect_uri", &pending.redirect_uri),
                ("resource", RESOURCE),
            ])
            .await?;
        let id_token = token
            .id_token
            .clone()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| LlmError::login("token response did not contain an ID token"))?;
        let discovery = self.discovery().await?;
        let claims = verify_id_token(
            &self.http,
            &discovery.jwks_uri,
            &id_token,
            &IdTokenCheck {
                issuer: &self.endpoints.issuer,
                client_id: &client_id,
                nonce: &pending.nonce,
            },
        )
        .await?;
        if let Some(subject) = &pending.subject
            && subject != &claims.sub
        {
            return Err(LlmError::login(
                "signed in as a different account than the one selected",
            ));
        }
        let tokens = tokens_from_response(token, None)?;
        Ok(ChatGptAccount {
            client_id,
            issuer: claims.iss,
            subject: claims.sub,
            email: claims.email,
            id_token: Some(id_token),
            tokens: Some(tokens),
        })
    }

    pub async fn refresh(&self, account: &ChatGptAccount) -> Result<OAuthTokens> {
        let current = account
            .tokens
            .as_ref()
            .ok_or_else(|| LlmError::NotAuthenticated {
                provider: "openai".into(),
                message: "no refresh token; sign in again".into(),
            })?;
        let token = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &account.client_id),
                ("refresh_token", &current.refresh_token),
                ("resource", RESOURCE),
            ])
            .await?;
        tokens_from_response(token, Some(&current.refresh_token))
    }

    pub async fn revoke(&self, account: &ChatGptAccount) -> Result<()> {
        let Some(tokens) = &account.tokens else {
            return Ok(());
        };
        let discovery = self.discovery().await?;
        let Some(endpoint) = discovery.revocation_endpoint else {
            return Err(LlmError::login(
                "issuer does not publish a revocation endpoint",
            ));
        };
        self.http
            .post(endpoint)
            .form(&[
                ("token", tokens.refresh_token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", account.client_id.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn discovery(&self) -> Result<Discovery> {
        Ok(self
            .http
            .get(&self.endpoints.discovery_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let response = self
            .http
            .post(&self.endpoints.token_url)
            .header("accept", "application/json")
            .form(form)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(token_error("openai", status.as_u16(), &body));
        }
        Ok(response.json().await?)
    }
}

pub(crate) fn token_error(provider: &str, status: u16, body: &str) -> LlmError {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let code = parsed.as_ref().and_then(|v| {
        v["error"]
            .as_str()
            .or_else(|| v["error"]["code"].as_str())
            .map(str::to_owned)
    });
    let message = parsed
        .as_ref()
        .and_then(|v| {
            v["error_description"]
                .as_str()
                .or_else(|| v["error"]["message"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned());
    if matches!(code.as_deref(), Some("invalid_grant" | "invalid_client")) {
        return LlmError::NotAuthenticated {
            provider: provider.to_owned(),
            message: format!("{message}; sign in again"),
        };
    }
    LlmError::Api {
        provider: provider.to_owned(),
        status,
        code,
        message: format!("token request failed: {message}"),
        retry_after: None,
    }
}

fn tokens_from_response(
    token: TokenResponse,
    previous_refresh: Option<&str>,
) -> Result<OAuthTokens> {
    let access_token = token
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| LlmError::login("token response has no access_token"))?;
    let refresh_token = token
        .refresh_token
        .filter(|t| !t.trim().is_empty())
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| LlmError::login("token response has no refresh_token"))?;
    let expires_in = token
        .expires_in
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| LlmError::login("token response has an invalid expires_in"))?;
    let scopes: Vec<String> = token
        .scope
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if !scopes.iter().any(|s| s == DIRECT_SCOPE) {
        return Err(LlmError::login(format!(
            "the grant does not include {DIRECT_SCOPE}; ChatGPT plan usage is not enabled for this account"
        )));
    }
    let now = now_unix();
    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at: now + expires_in as u64,
        scopes,
        saved_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_redirect_uri() {
        check_redirect_uri("http://127.0.0.1:1455/auth/callback").unwrap();
        check_redirect_uri("http://127.0.0.1:54321/auth/callback").unwrap();
        assert!(check_redirect_uri("http://localhost:1455/auth/callback").is_err());
        assert!(check_redirect_uri("http://127.0.0.1:1455/callback").is_err());
        assert!(check_redirect_uri("https://127.0.0.1:1455/auth/callback").is_err());
    }

    #[test]
    fn first_login_uses_dynamic_client() {
        let client = SiwcClient::new(reqwest::Client::new());
        let pending = client
            .start(&loopback_redirect_uri(1455), "urn:uuid:x", None)
            .unwrap();
        let url = Url::parse(&pending.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/api/accounts/authorize");
        assert_eq!(q["client_id"], DYNAMIC_CLIENT_ID);
        assert_eq!(q["agent_name_hint"], AGENT_NAME);
        assert_eq!(q["ext_agent_host_id"], "urn:uuid:x");
        assert_eq!(q["resource"], RESOURCE);
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], pending.state);
        assert_eq!(q["nonce"], pending.nonce);
    }

    #[test]
    fn reauth_uses_saved_client_and_hints() {
        let client = SiwcClient::new(reqwest::Client::new());
        let account = ChatGptAccount {
            client_id: "oaiapp_1".into(),
            issuer: "https://auth.openai.com".into(),
            subject: "user-1".into(),
            email: Some("u@example.com".into()),
            id_token: Some("old.id.token".into()),
            tokens: None,
        };
        let pending = client
            .start(&loopback_redirect_uri(5000), "urn:uuid:x", Some(&account))
            .unwrap();
        let q: std::collections::HashMap<_, _> = Url::parse(&pending.url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(q["client_id"], "oaiapp_1");
        assert_eq!(q["id_token_hint"], "old.id.token");
        assert_eq!(q["login_hint"], "u@example.com");
        assert!(!q.contains_key("agent_name_hint"));
    }

    #[test]
    fn rejects_grant_without_direct_scope() {
        let token = TokenResponse {
            access_token: Some("a".into()),
            refresh_token: Some("r".into()),
            id_token: None,
            expires_in: Some(3600.0),
            scope: Some("openid profile".into()),
        };
        assert!(tokens_from_response(token, None).is_err());
    }
}
