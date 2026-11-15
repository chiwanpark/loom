//! Sends agents' notifications through Firebase Cloud Messaging, with the
//! HTTP v1 API and a service account of the app's Firebase project.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use loom_config::{FCM_API_URL, FcmConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// How long an access token is asked for; Google allows an hour at most.
const TOKEN_LIFETIME: Duration = Duration::from_secs(3600);
/// A token is renewed this long before it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(120);
/// How long FCM keeps a notification for a phone that is offline.
const TTL: &str = "86400s";

/// The parts of a service account's JSON key that sending needs.
#[derive(Deserialize)]
struct ServiceAccount {
    project_id: String,
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

/// What became of a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// FCM says the token is not, or no longer, a valid registration token.
    Rejected,
    Failed(String),
}

pub struct FcmSender {
    http: reqwest::Client,
    account: ServiceAccount,
    key: EncodingKey,
    send_url: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl FcmSender {
    pub fn load(config: &FcmConfig) -> Result<Self, String> {
        let path = config.service_account.display();
        let text = std::fs::read_to_string(&config.service_account)
            .map_err(|e| format!("cannot read the service account key {path}: {e}"))?;
        let account: ServiceAccount = serde_json::from_str(&text)
            .map_err(|e| format!("{path} is not a service account key: {e}"))?;
        let key = EncodingKey::from_rsa_pem(account.private_key.as_bytes())
            .map_err(|e| format!("the private key in {path} is not usable: {e}"))?;
        let api = config
            .api_url
            .as_deref()
            .unwrap_or(FCM_API_URL)
            .trim_end_matches('/');
        let send_url = format!("{api}/v1/projects/{}/messages:send", account.project_id);
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            account,
            key,
            send_url,
            token: Mutex::new(None),
        })
    }

    pub fn project(&self) -> &str {
        &self.account.project_id
    }

    /// An access token for FCM, from the cache while it is fresh.
    async fn access_token(&self) -> Result<String, String> {
        let mut cached = self.token.lock().await;
        if let Some((token, expires)) = cached.as_ref()
            && Instant::now() + TOKEN_MARGIN < *expires
        {
            return Ok(token.clone());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let claims = Claims {
            iss: &self.account.client_email,
            scope: SCOPE,
            aud: &self.account.token_uri,
            iat: now,
            exp: now + TOKEN_LIFETIME.as_secs(),
        };
        let assertion = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|e| format!("cannot sign the token request: {e}"))?;
        let response: TokenResponse = self
            .http
            .post(&self.account.token_uri)
            .form(&[
                ("grant_type", JWT_BEARER),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("cannot get an access token for FCM: {e}"))?
            .json()
            .await
            .map_err(|e| format!("malformed access token response: {e}"))?;
        let lifetime = response
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(TOKEN_LIFETIME);
        *cached = Some((response.access_token.clone(), Instant::now() + lifetime));
        Ok(response.access_token)
    }

    /// Sends `data` to the app with the registration token `token`, as a
    /// high-priority data message, which the app shows as it decides.
    pub async fn send(&self, token: &str, data: &[(&str, String)]) -> Outcome {
        let access = match self.access_token().await {
            Ok(access) => access,
            Err(err) => return Outcome::Failed(err),
        };
        let data: serde_json::Map<String, serde_json::Value> = data
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String(v.clone())))
            .collect();
        let body = serde_json::json!({
            "message": {
                "token": token,
                "data": data,
                "android": { "priority": "HIGH", "ttl": TTL },
            }
        });
        let response = match self
            .http
            .post(&self.send_url)
            .bearer_auth(&access)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => return Outcome::Failed(format!("cannot reach FCM: {err}")),
        };
        let status = response.status();
        if status.is_success() {
            return Outcome::Sent;
        }
        let text = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // The token may have been revoked early; the next send gets a new one.
            *self.token.lock().await = None;
        }
        if is_dead_token(status, &text) {
            Outcome::Rejected
        } else {
            Outcome::Failed(format!("FCM answered {status}: {}", text.trim()))
        }
    }
}

/// Whether FCM's error says the registration token is no good: it is
/// unregistered (404 `UNREGISTERED`), or not a token at all (400
/// `INVALID_ARGUMENT` about the registration token).
fn is_dead_token(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::NOT_FOUND
        || body.contains("UNREGISTERED")
        || (status == reqwest::StatusCode::BAD_REQUEST && body.contains("registration token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_tokens_are_told_from_other_errors() {
        use reqwest::StatusCode;
        assert!(is_dead_token(StatusCode::NOT_FOUND, ""));
        assert!(is_dead_token(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"The registration token is not a valid FCM registration token"}}"#
        ));
        assert!(!is_dead_token(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"Invalid JSON payload"}}"#
        ));
        assert!(!is_dead_token(StatusCode::SERVICE_UNAVAILABLE, "busy"));
        assert!(!is_dead_token(StatusCode::UNAUTHORIZED, ""));
    }
}
