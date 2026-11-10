pub(crate) mod callback;
pub(crate) mod jwt;
pub(crate) mod pkce;

use crate::error::LlmError;

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
