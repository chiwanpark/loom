use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use loom_config::AccessVerifyConfig;
use tokio::sync::Mutex;

pub const JWT_HEADER: &str = "cf-access-jwt-assertion";
const MIN_REFRESH: Duration = Duration::from_secs(30);

struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
}

pub struct AccessVerifier {
    config: AccessVerifyConfig,
    http: reqwest::Client,
    cache: Mutex<Cache>,
}

#[derive(serde::Deserialize)]
struct Claims {}

impl AccessVerifier {
    pub fn new(config: AccessVerifyConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            cache: Mutex::new(Cache {
                keys: HashMap::new(),
                fetched: None,
            }),
        }
    }

    async fn refresh(&self, cache: &mut Cache) -> Result<(), String> {
        if cache.fetched.is_some_and(|t| t.elapsed() < MIN_REFRESH) {
            return Ok(());
        }
        cache.fetched = Some(Instant::now());
        let jwks: JwkSet = self
            .http
            .get(self.config.certs_url())
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("cannot fetch Access signing keys: {e}"))?
            .json()
            .await
            .map_err(|e| format!("malformed Access signing keys: {e}"))?;
        cache.keys = jwks
            .keys
            .iter()
            .filter_map(|jwk| {
                let kid = jwk.common.key_id.clone()?;
                DecodingKey::from_jwk(jwk).ok().map(|key| (kid, key))
            })
            .collect();
        Ok(())
    }

    pub async fn verify(&self, token: &str) -> Result<(), String> {
        let header = decode_header(token).map_err(|e| format!("malformed Access token: {e}"))?;
        if header.alg != Algorithm::RS256 {
            return Err(format!(
                "unexpected Access token algorithm {:?}",
                header.alg
            ));
        }
        let kid = header.kid.ok_or("the Access token has no key ID")?;
        let key = {
            let mut cache = self.cache.lock().await;
            if !cache.keys.contains_key(&kid) {
                self.refresh(&mut cache).await?;
            }
            cache
                .keys
                .get(&kid)
                .cloned()
                .ok_or("the Access token was signed by an unknown key")?
        };
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.config.audience]);
        validation.set_issuer(&[self.config.issuer()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        decode::<Claims>(token, &key, &validation)
            .map(|_| ())
            .map_err(|e| format!("invalid Access token: {e}"))
    }
}
