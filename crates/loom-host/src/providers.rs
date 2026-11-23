use loom_client::oracle_proto::{
    CheckResult, ErrorCode, OracleError, ProviderRequest, WorkspaceCheck, WorkspaceCheckResult,
};
use loom_github::{ApiRequest, Http, Method};
use serde_json::Value;

const MAX_PROVIDERS: usize = 50;

fn bad_request(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::BadRequest, message)
}

fn valid_api_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && !rest.contains(['?', '#', '@', '\\'])
            && rest.chars().all(|c| c.is_ascii_graphic())
    })
}

pub async fn check(http: &Http, check: &WorkspaceCheck) -> WorkspaceCheckResult {
    let mut result = WorkspaceCheckResult::default();
    for provider in check.providers.iter().take(MAX_PROVIDERS) {
        let error = if valid_api_url(&provider.api_url) {
            http.reach(&provider.api_url).await.err()
        } else {
            Some(format!("{:?} is not a URL of an API", provider.api_url))
        };
        if let Some(error) = &error {
            tracing::info!(provider = %provider.name, %error, "cannot reach a Git provider");
        }
        result.providers.push(CheckResult {
            name: provider.name.clone(),
            error,
        });
    }
    result
}

pub async fn request(http: &Http, request: &ProviderRequest) -> Result<Value, OracleError> {
    if !valid_api_url(&request.api_url) {
        return Err(bad_request(format!(
            "{:?} is not a URL of an API",
            request.api_url
        )));
    }
    let method = Method::parse(&request.method)
        .ok_or_else(|| bad_request(format!("{:?} is not a method", request.method)))?;
    let api = ApiRequest {
        method,
        path: request.path.clone(),
        query: request.query.clone(),
        body: request.body.clone(),
    };
    let token = request.token.as_ref().map(|t| t.expose());
    let response = http
        .send(&request.api_url, token, &api)
        .await
        .map_err(|err| OracleError::new(ErrorCode::Unavailable, err.to_string()))?;
    tracing::debug!(api = %request.api_url, method = %request.method, path = %request.path, status = response.status, "sent a request to a Git provider");
    serde_json::to_value(response)
        .map_err(|err| OracleError::new(ErrorCode::Internal, err.to_string()))
}

#[cfg(test)]
mod tests {
    use loom_client::oracle_proto::{ProviderEndpoint, Secret};

    use super::*;

    #[tokio::test]
    async fn bad_urls_are_not_reached() {
        let http = Http::new().unwrap();
        let result = check(
            &http,
            &WorkspaceCheck {
                providers: vec![
                    ProviderEndpoint {
                        name: "bad".into(),
                        api_url: "file:///etc/passwd".into(),
                    },
                    ProviderEndpoint {
                        name: "closed".into(),
                        api_url: "http://127.0.0.1:1".into(),
                    },
                ],
                directories: Vec::new(),
            },
        )
        .await;
        assert_eq!(result.providers.len(), 2);
        assert!(result.providers.iter().all(|p| p.error.is_some()));

        let bad = ProviderRequest {
            api_url: "https://user@evil.example".into(),
            token: Some(Secret::new("t")),
            method: "GET".into(),
            path: "/user".into(),
            query: Vec::new(),
            body: None,
        };
        assert_eq!(
            request(&http, &bad).await.unwrap_err().code,
            ErrorCode::BadRequest
        );
        let delete = ProviderRequest {
            api_url: "https://api.example".into(),
            method: "DELETE".into(),
            ..bad
        };
        assert_eq!(
            request(&http, &delete).await.unwrap_err().code,
            ErrorCode::BadRequest
        );
    }
}
