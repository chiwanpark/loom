use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{AlgorithmFamily, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{LlmError, Result};

pub fn unverified_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IdTokenClaims {
    pub sub: String,
    pub iss: String,
    pub exp: u64,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
}

pub struct IdTokenCheck<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
}

pub async fn verify_id_token(
    http: &reqwest::Client,
    jwks_url: &str,
    token: &str,
    check: &IdTokenCheck<'_>,
) -> Result<IdTokenClaims> {
    let header = decode_header(token)
        .map_err(|e| LlmError::login(format!("invalid ID token header: {e}")))?;
    if !matches!(
        header.alg.family(),
        AlgorithmFamily::Rsa | AlgorithmFamily::Ec | AlgorithmFamily::Ed
    ) {
        return Err(LlmError::login(format!(
            "ID token uses unsupported algorithm {:?}",
            header.alg
        )));
    }
    let jwks: JwkSet = http
        .get(jwks_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let jwk = match &header.kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
    .ok_or_else(|| LlmError::login("ID token signing key not found in JWKS"))?;
    let key = DecodingKey::from_jwk(jwk)
        .map_err(|e| LlmError::login(format!("invalid JWKS key: {e}")))?;
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[check.client_id]);
    validation.set_issuer(&[check.issuer]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let data = decode::<IdTokenClaims>(token, &key, &validation)
        .map_err(|e| LlmError::login(format!("ID token validation failed: {e}")))?;
    if data.claims.nonce.as_deref() != Some(check.nonce) {
        return Err(LlmError::login("ID token nonce mismatch"));
    }
    Ok(data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_payload_without_verification() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"u1","n":1}"#);
        let token = format!("e30.{payload}.sig");
        let claims = unverified_claims(&token).unwrap();
        assert_eq!(claims["sub"], "u1");
        assert!(unverified_claims("garbage").is_none());
    }
}
