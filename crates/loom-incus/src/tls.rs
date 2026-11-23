use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::IncusError;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn tls(message: impl std::fmt::Display) -> IncusError {
    IncusError::Tls(message.to_string())
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn read_certificate(path: &Path) -> Result<Vec<u8>, IncusError> {
    let first = CertificateDer::pem_file_iter(path)
        .map_err(|err| tls(format!("cannot read {}: {err}", path.display())))?
        .next()
        .ok_or_else(|| tls(format!("{} has no certificate", path.display())))?
        .map_err(|err| tls(format!("cannot read {}: {err}", path.display())))?;
    Ok(first.to_vec())
}

pub fn certificate_pem(der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(chunk));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

#[derive(Debug)]
struct Pinned {
    der: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

type Seen = Arc<Mutex<Option<Vec<u8>>>>;

#[derive(Debug)]
struct Capture {
    seen: Seen,
    provider: Arc<CryptoProvider>,
}

fn verify12(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    verify_tls12_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

fn verify13(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    verify_tls13_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.der.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "the server certificate {} is not the pinned certificate {}",
                fingerprint(end_entity),
                fingerprint(&self.der)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify12(&self.provider, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify13(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ServerCertVerifier for Capture {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(end_entity.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify12(&self.provider, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify13(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub enum ServerTrust {
    Pinned(Vec<u8>),
    System,
}

fn builder() -> Result<rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier>, IncusError> {
    ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(tls)
}

pub fn client_config(
    certificate: &Path,
    key: &Path,
    trust: ServerTrust,
) -> Result<ClientConfig, IncusError> {
    let chain = CertificateDer::pem_file_iter(certificate)
        .map_err(|err| tls(format!("cannot read {}: {err}", certificate.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| tls(format!("cannot read {}: {err}", certificate.display())))?;
    if chain.is_empty() {
        return Err(tls(format!("{} has no certificate", certificate.display())));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|err| tls(format!("cannot read {}: {err}", key.display())))?;
    let builder = builder()?;
    let builder = match trust {
        ServerTrust::Pinned(der) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned {
                der,
                provider: provider(),
            })),
        ServerTrust::System => {
            let mut roots = rustls::RootCertStore::empty();
            let found = rustls_native_certs::load_native_certs();
            for cert in found.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(tls("the system has no CA certificates"));
            }
            builder.with_root_certificates(roots)
        }
    };
    builder.with_client_auth_cert(chain, key).map_err(tls)
}

pub(crate) fn capture_config() -> Result<(ClientConfig, Seen), IncusError> {
    let seen = Arc::new(Mutex::new(None));
    let config = builder()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Capture {
            seen: seen.clone(),
            provider: provider(),
        }))
        .with_no_client_auth();
    Ok((config, seen))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TrustToken {
    #[serde(default)]
    pub client_name: String,
    pub fingerprint: String,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl TrustToken {
    pub fn parse(token: &str) -> Result<Self, IncusError> {
        let token = token.trim();
        let engines = [
            base64::engine::general_purpose::STANDARD,
            base64::engine::general_purpose::URL_SAFE,
            base64::engine::general_purpose::STANDARD_NO_PAD,
            base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ];
        let bytes = engines
            .iter()
            .find_map(|engine| engine.decode(token).ok())
            .ok_or_else(|| IncusError::Protocol("the trust token is not base64".into()))?;
        serde_json::from_slice(&bytes)
            .map_err(|err| IncusError::Protocol(format!("the trust token is not valid: {err}")))
    }
}

pub struct GeneratedCertificate {
    pub certificate: String,
    pub key: String,
}

pub fn generate_certificate(name: &str) -> Result<GeneratedCertificate, IncusError> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).map_err(tls)?;
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()]).map_err(tls)?;
    let mut subject = rcgen::DistinguishedName::new();
    subject.push(rcgen::DnType::CommonName, name);
    params.distinguished_name = subject;
    let year = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|age| 1970 + (age.as_secs() / 31_556_952) as i32)
        .unwrap_or(2026);
    params.not_before = rcgen::date_time_ymd(year - 1, 1, 1);
    params.not_after = rcgen::date_time_ymd(year + 10, 1, 1);
    let certificate = params.self_signed(&key).map_err(tls)?;
    Ok(GeneratedCertificate {
        certificate: certificate.pem(),
        key: key.serialize_pem(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_name_the_server_certificate() {
        let json = r#"{"client_name":"loom","fingerprint":"ab12","addresses":["10.0.0.1:8443"],"secret":"s","expires_at":"0001-01-01T00:00:00Z"}"#;
        let token = base64::engine::general_purpose::STANDARD.encode(json);
        let parsed = TrustToken::parse(&format!("{token}\n")).unwrap();
        assert_eq!(parsed.client_name, "loom");
        assert_eq!(parsed.fingerprint, "ab12");
        assert_eq!(parsed.addresses, vec!["10.0.0.1:8443".to_owned()]);
        assert!(TrustToken::parse("not a token").is_err());
    }

    #[test]
    fn generated_certificates_load_as_client_certificates() {
        let dir = tempfile::tempdir().unwrap();
        let generated = generate_certificate("loom-host").unwrap();
        let cert = dir.path().join("client.crt");
        let key = dir.path().join("client.key");
        std::fs::write(&cert, &generated.certificate).unwrap();
        std::fs::write(&key, &generated.key).unwrap();
        let der = read_certificate(&cert).unwrap();
        assert_eq!(certificate_pem(&der), generated.certificate);
        assert_eq!(fingerprint(&der).len(), 64);
        client_config(&cert, &key, ServerTrust::Pinned(der)).unwrap();
        assert!(client_config(&key, &key, ServerTrust::System).is_err());
    }
}
