use std::time::Duration;

use reqwest::{RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::ID;
use crate::error::{LlmError, Result};
use crate::kind::Shared;
use crate::store::now_unix;

const GITHUB: &str = "https://github.com";
const GITHUB_API: &str = "https://api.github.com";
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_MARGIN: u64 = 60;
pub(super) const INDIVIDUAL_API: &str = "https://api.individual.githubcopilot.com";

pub(super) fn headers(builder: RequestBuilder) -> RequestBuilder {
    builder
        .header("user-agent", USER_AGENT)
        .header("editor-version", "vscode/1.107.0")
        .header("editor-plugin-version", "copilot-chat/0.35.0")
        .header("copilot-integration-id", "vscode-chat")
}

#[derive(Serialize, Deserialize)]
pub(super) struct Credential {
    github_token: String,
    pub token: String,
    expires_at: u64,
}

impl Credential {
    fn is_fresh(&self) -> bool {
        !self.token.is_empty() && self.expires_at > now_unix().saturating_add(REFRESH_MARGIN)
    }

    pub fn api_base(&self) -> &'static str {
        let proxy = self
            .token
            .split(';')
            .find_map(|part| part.strip_prefix("proxy-ep="));
        match proxy {
            Some("proxy.business.githubcopilot.com") => "https://api.business.githubcopilot.com",
            Some("proxy.enterprise.githubcopilot.com") => {
                "https://api.enterprise.githubcopilot.com"
            }
            _ => INDIVIDUAL_API,
        }
    }
}

#[derive(Deserialize)]
struct CopilotToken {
    token: String,
    expires_at: u64,
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

pub(super) struct Device {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    code: String,
    interval: Duration,
    deadline: Instant,
}

#[derive(Deserialize)]
struct DeviceToken {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

fn not_signed_in() -> LlmError {
    LlmError::NotAuthenticated {
        provider: ID.to_string(),
        message: "sign in to GitHub Copilot first".into(),
    }
}

fn oauth_request(shared: &Shared, path: &str) -> RequestBuilder {
    let base = shared.auth_base_url(&ID, GITHUB);
    shared
        .http
        .post(format!("{}{path}", base.trim_end_matches('/')))
        .header("accept", "application/json")
        .header("user-agent", USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
}

pub(super) async fn start(shared: &Shared) -> Result<Device> {
    let response = oauth_request(shared, "/login/device/code")
        .form(&[("client_id", CLIENT_ID), ("scope", "read:user")])
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(LlmError::login(format!(
            "GitHub device code request failed ({status})"
        )));
    }
    let code: DeviceCode = response.json().await?;
    if code.device_code.is_empty() || code.user_code.is_empty() || code.expires_in == 0 {
        return Err(LlmError::login("invalid GitHub device code response"));
    }
    let url = Url::parse(&code.verification_uri)
        .ok()
        .filter(|url| {
            url.scheme() == "https"
                && url.origin().ascii_serialization() == GITHUB
                && url.username().is_empty()
                && url.password().is_none()
        })
        .ok_or_else(|| LlmError::login("invalid GitHub device verification URL"))?;
    let expires_in = code.expires_in.min(20 * 60);
    Ok(Device {
        user_code: code.user_code,
        verification_uri: url.into(),
        expires_in,
        code: code.device_code,
        interval: Duration::from_secs(code.interval.unwrap_or(5).max(1)),
        deadline: Instant::now() + Duration::from_secs(expires_in),
    })
}

async fn poll(shared: &Shared, device: &Device) -> Result<String> {
    let mut interval = device.interval;
    loop {
        tokio::time::sleep(interval).await;
        let response = oauth_request(shared, "/login/oauth/access_token")
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", &device.code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let status = response.status();
        let body = response.json::<DeviceToken>().await;
        match body.as_ref().ok().and_then(|body| body.error.as_deref()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval =
                    interval
                        .saturating_add(Duration::from_secs(5))
                        .max(Duration::from_secs(
                            body.ok().and_then(|body| body.interval).unwrap_or(0),
                        ));
            }
            Some("expired_token") => return Err(expired()),
            Some("access_denied") => {
                return Err(LlmError::login("GitHub authorization was denied"));
            }
            Some(_) => {
                return Err(LlmError::login(
                    "GitHub rejected the device code; start the login again",
                ));
            }
            _ if !status.is_success() => {
                return Err(LlmError::login(format!(
                    "GitHub device login failed ({status})"
                )));
            }
            _ => {
                return body?
                    .access_token
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| LlmError::login("GitHub did not issue an access token"));
            }
        }
    }
}

fn expired() -> LlmError {
    LlmError::login("GitHub device code expired; start the login again")
}

pub(super) async fn wait(
    shared: &Shared,
    device: &Device,
    cancel: &CancellationToken,
) -> Result<Credential> {
    let sign_in = async {
        let github_token = tokio::time::timeout_at(device.deadline, poll(shared, device))
            .await
            .map_err(|_| expired())??;
        exchange(shared, github_token).await
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(LlmError::Cancelled),
        result = sign_in => result,
    }
}

async fn exchange(shared: &Shared, github_token: String) -> Result<Credential> {
    let base = shared.auth_base_url(&ID, GITHUB_API);
    let response = headers(shared.http.get(format!(
        "{}/copilot_internal/v2/token",
        base.trim_end_matches('/')
    )))
    .header("accept", "application/json")
    .bearer_auth(&github_token)
    .timeout(REQUEST_TIMEOUT)
    .send()
    .await?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(not_signed_in());
    }
    if matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        return Err(LlmError::NotAuthenticated {
            provider: ID.to_string(),
            message: "check that your GitHub account has Copilot access, then sign in again".into(),
        });
    }
    if !status.is_success() {
        return Err(LlmError::Api {
            provider: ID.to_string(),
            status: status.as_u16(),
            code: None,
            message: "Copilot token exchange failed".into(),
            retry_after: None,
        });
    }
    let body: CopilotToken = response.json().await?;
    if body.token.is_empty() || body.expires_at <= now_unix() {
        return Err(LlmError::protocol(
            ID.as_str(),
            "invalid or expired Copilot token",
        ));
    }
    Ok(Credential {
        github_token,
        token: body.token,
        expires_at: body.expires_at,
    })
}

pub(super) async fn credential(shared: &Shared) -> Result<Credential> {
    let credential: Credential = shared
        .store
        .load_section(ID.as_str())
        .await?
        .ok_or_else(not_signed_in)?;
    if credential.is_fresh() {
        return Ok(credential);
    }
    let guard = shared.store.lock().await?;
    let credential: Credential = guard
        .load_section(ID.as_str())
        .await?
        .ok_or_else(not_signed_in)?;
    if credential.is_fresh() {
        return Ok(credential);
    }
    let refreshed = exchange(shared, credential.github_token).await?;
    guard.save_section(ID.as_str(), Some(&refreshed)).await?;
    Ok(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_uses_only_known_hosts() {
        for (proxy, host) in [
            ("proxy.business.githubcopilot.com", "business"),
            ("proxy.enterprise.githubcopilot.com", "enterprise"),
            ("proxy.individual.githubcopilot.com", "individual"),
            (
                "proxy.business.githubcopilot.com.evil.example",
                "individual",
            ),
            (
                "proxy.business.githubcopilot.com@evil.example",
                "individual",
            ),
        ] {
            let credential = Credential {
                github_token: "github-token".into(),
                token: format!("tid=test;proxy-ep={proxy};exp=9999999999"),
                expires_at: 9999999999,
            };
            assert_eq!(
                credential.api_base(),
                format!("https://api.{host}.githubcopilot.com")
            );
        }
    }
}
