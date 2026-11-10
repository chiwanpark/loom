use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{LlmError, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

impl CallbackParams {
    pub fn from_query(query: &str) -> Self {
        let mut params = Self::default();
        for (key, value) in url::form_urlencoded::parse(query.trim_start_matches('?').as_bytes()) {
            let value = Some(value.trim().to_owned()).filter(|v| !v.is_empty());
            match key.as_ref() {
                "code" => params.code = value,
                "state" => params.state = value,
                "client_id" => params.client_id = value,
                "error" => params.error = value,
                "error_description" => params.error_description = value,
                _ => {}
            }
        }
        params
    }

    pub fn from_redirect_url(input: &str, redirect_uri: &str) -> Result<Self> {
        let url = Url::parse(input.trim())
            .map_err(|_| LlmError::login("paste the full callback URL from the browser"))?;
        let expected = Url::parse(redirect_uri)
            .map_err(|e| LlmError::login(format!("invalid redirect URI: {e}")))?;
        if url.origin() != expected.origin() || url.path() != expected.path() {
            return Err(LlmError::login(format!(
                "the callback URL must start with {redirect_uri}"
            )));
        }
        Ok(Self::from_query(url.query().unwrap_or_default()))
    }

    pub(crate) fn check_state(&self, expected: &str) -> Result<()> {
        match self.state.as_deref() {
            None => Err(LlmError::login("missing OAuth state")),
            Some(state) if state != expected => Err(LlmError::login("OAuth state mismatch")),
            Some(_) => Ok(()),
        }
    }

    pub(crate) fn check_error(&self) -> Result<()> {
        match &self.error {
            Some(error) => Err(LlmError::login(match &self.error_description {
                Some(desc) => format!("authorization failed: {error}: {desc}"),
                None => format!("authorization failed: {error}"),
            })),
            None => Ok(()),
        }
    }

    pub(crate) fn require_code(&self) -> Result<&str> {
        self.code
            .as_deref()
            .ok_or_else(|| LlmError::login("missing authorization code"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_redirect_url() {
        let params = CallbackParams::from_redirect_url(
            "http://127.0.0.1:1455/auth/callback?code=abc&state=s1&client_id=oaiapp_1",
            "http://127.0.0.1:1455/auth/callback",
        )
        .unwrap();
        assert_eq!(params.code.as_deref(), Some("abc"));
        assert_eq!(params.state.as_deref(), Some("s1"));
        assert_eq!(params.client_id.as_deref(), Some("oaiapp_1"));
        params.check_state("s1").unwrap();
        assert!(params.check_state("s2").is_err());
    }

    #[test]
    fn rejects_other_origin_or_path() {
        let redirect = "http://127.0.0.1:1455/auth/callback";
        assert!(
            CallbackParams::from_redirect_url(
                "http://127.0.0.1:1456/auth/callback?code=a",
                redirect
            )
            .is_err()
        );
        assert!(
            CallbackParams::from_redirect_url("http://127.0.0.1:1455/callback?code=a", redirect)
                .is_err()
        );
        assert!(CallbackParams::from_redirect_url("not a url", redirect).is_err());
    }

    #[test]
    fn reports_oauth_error() {
        let params = CallbackParams::from_query("error=access_denied&state=s");
        let err = params.check_error().unwrap_err().to_string();
        assert!(err.contains("access_denied"));
    }
}
