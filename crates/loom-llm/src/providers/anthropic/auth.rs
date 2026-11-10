use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::ID;
use crate::HttpClient;
use crate::error::{LlmError, Result};
use crate::oauth::callback::CallbackParams;
use crate::oauth::pkce::Pkce;
use crate::oauth::token_error;
use crate::store::now_unix;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_BASE: &str = "https://platform.claude.com";
const COPY_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub(super) const CALLBACK_PORT: u16 = 53692;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: Option<String>,
    pub label: Option<String>,
}

impl Credential {
    pub fn is_fresh(&self) -> bool {
        !self.access_token.is_empty() && self.expires_at > now_unix().saturating_add(300)
    }
}

pub(super) struct Pending {
    pkce: Pkce,
    pub redirect_uri: String,
    pub url: String,
}

#[derive(Clone)]
pub(super) struct Client {
    http: HttpClient,
    base: String,
}

impl Client {
    pub fn new(http: HttpClient, base: String) -> Self {
        Self { http, base }
    }

    pub fn default_base() -> &'static str {
        TOKEN_BASE
    }

    pub fn start(&self, redirect: Option<String>) -> Result<Pending> {
        let redirect_uri = redirect.unwrap_or_else(|| COPY_REDIRECT.into());
        let redirect = Url::parse(&redirect_uri)
            .map_err(|_| LlmError::login("invalid Anthropic redirect URI"))?;
        let loopback = redirect.scheme() == "http"
            && matches!(redirect.host_str(), Some("localhost" | "127.0.0.1"))
            && redirect.port().is_some()
            && redirect.path() == "/callback"
            && redirect.username().is_empty()
            && redirect.password().is_none()
            && redirect.query().is_none()
            && redirect.fragment().is_none();
        if !loopback && redirect_uri != COPY_REDIRECT {
            return Err(LlmError::login(
                "Anthropic requires a loopback /callback redirect or its copy-code callback",
            ));
        }
        let pkce = Pkce::generate();
        let authorize = if self.base.trim_end_matches('/') == TOKEN_BASE {
            AUTHORIZE_URL.to_owned()
        } else {
            format!("{}/oauth/authorize", self.base.trim_end_matches('/'))
        };
        let mut url = Url::parse(&authorize)
            .map_err(|e| LlmError::login(format!("invalid authorization URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("code", "true")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &pkce.verifier);
        Ok(Pending {
            pkce,
            redirect_uri,
            url: url.into(),
        })
    }

    pub async fn complete(&self, pending: &Pending, text: &str) -> Result<Credential> {
        let text = text.trim();
        let params = if text.contains("://") {
            let params = CallbackParams::from_redirect_url(text, &pending.redirect_uri)?;
            params.check_state(&pending.pkce.verifier)?;
            params
        } else if let Some((code, state)) = text.split_once('#') {
            CallbackParams {
                code: Some(code.into()),
                state: Some(state.into()),
                ..Default::default()
            }
        } else if text.contains("code=") || text.starts_with("error=") {
            CallbackParams::from_query(text)
        } else {
            CallbackParams {
                code: Some(text.into()),
                ..Default::default()
            }
        };
        if params.state.is_some() {
            params.check_state(&pending.pkce.verifier)?;
        }
        params.check_error()?;
        let code = params.require_code()?;
        if code.trim().is_empty() {
            return Err(LlmError::login("missing authorization code"));
        }
        self.token(
            json!({
                "grant_type": "authorization_code",
                "client_id": CLIENT_ID,
                "code": code,
                "state": pending.pkce.verifier,
                "redirect_uri": pending.redirect_uri,
                "code_verifier": pending.pkce.verifier,
            }),
            None,
        )
        .await
    }

    pub async fn refresh(&self, credential: &Credential) -> Result<Credential> {
        if credential.refresh_token.is_empty() {
            return Err(LlmError::NotAuthenticated {
                provider: ID.to_string(),
                message: "missing refresh token; sign in again".into(),
            });
        }
        self.token(
            json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": credential.refresh_token,
            }),
            Some(credential),
        )
        .await
    }

    async fn token(&self, body: Value, previous: Option<&Credential>) -> Result<Credential> {
        let response = self
            .http
            .post(format!(
                "{}/v1/oauth/token",
                self.base.trim_end_matches('/')
            ))
            .timeout(Duration::from_secs(30))
            .header("accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(token_error(ID.as_str(), status.as_u16(), &text));
        }
        let data: Value = serde_json::from_str(&text)
            .map_err(|_| LlmError::protocol(ID.as_str(), "invalid token response JSON"))?;
        let access_token = data["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LlmError::protocol(ID.as_str(), "missing access token"))?;
        let refresh_token = data["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| previous.map(|c| c.refresh_token.as_str()))
            .ok_or_else(|| LlmError::protocol(ID.as_str(), "missing refresh token"))?;
        let expires = data["expires_in"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(|| LlmError::protocol(ID.as_str(), "missing token lifetime"))?;
        Ok(Credential {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at: now_unix().saturating_add(expires),
            account_id: data["account"]["uuid"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| previous.and_then(|c| c.account_id.clone())),
            label: data["account"]["email_address"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| previous.and_then(|c| c.label.clone())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_login_endpoints_are_separate() {
        let client = Client::new(HttpClient::new(), Client::default_base().into());
        let pending = client.start(None).unwrap();
        assert!(
            pending
                .url
                .starts_with("https://claude.ai/oauth/authorize?")
        );
        assert_eq!(pending.redirect_uri, COPY_REDIRECT);
    }
}
