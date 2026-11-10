use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use loom_proto::relay::HostId;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::encoding::key_b64;
use crate::error::CryptoError;
use crate::files;

pub const HOST_IDENTITY_FILE: &str = "host_identity.json";
pub const CLIENT_IDENTITY_FILE: &str = "client_identity.json";

pub fn host_id_for(verifying_key: &[u8; 32]) -> HostId {
    HostId(*blake3::hash(verifying_key).as_bytes())
}

pub(crate) fn random_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    bytes
}

pub(crate) fn generate_static() -> Result<([u8; 32], [u8; 32]), CryptoError> {
    let builder = snow::Builder::new(crate::noise::IK_PARAMS.parse()?);
    let keypair = builder.generate_keypair()?;
    let private: [u8; 32] = keypair
        .private
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::Invalid("unexpected private key length".into()))?;
    let public: [u8; 32] = keypair
        .public
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::Invalid("unexpected public key length".into()))?;
    Ok((private, public))
}

#[derive(Serialize, Deserialize)]
struct HostFile {
    version: u32,
    #[serde(with = "key_b64")]
    signing_key: [u8; 32],
    #[serde(with = "key_b64")]
    static_private: [u8; 32],
    #[serde(with = "key_b64")]
    static_public: [u8; 32],
}

impl Drop for HostFile {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.signing_key.zeroize();
        self.static_private.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
struct ClientFile {
    version: u32,
    #[serde(with = "key_b64")]
    static_private: [u8; 32],
    #[serde(with = "key_b64")]
    static_public: [u8; 32],
}

impl Drop for ClientFile {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.static_private.zeroize();
    }
}

pub struct HostIdentity {
    signing: SigningKey,
    static_private: Zeroizing<[u8; 32]>,
    static_public: [u8; 32],
    path: PathBuf,
}

impl std::fmt::Debug for HostIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostIdentity")
            .field("host", &self.host_id().to_string())
            .finish_non_exhaustive()
    }
}

impl HostIdentity {
    pub fn load_or_create(data_dir: &Path) -> Result<Self, CryptoError> {
        let path = data_dir.join(HOST_IDENTITY_FILE);
        let _lock = files::lock(&path)?;
        if let Some(bytes) = files::read_optional(&path)? {
            let file: HostFile = serde_json::from_slice(&bytes)?;
            return Ok(Self {
                signing: SigningKey::from_bytes(&file.signing_key),
                static_private: Zeroizing::new(file.static_private),
                static_public: file.static_public,
                path,
            });
        }
        let (static_private, static_public) = generate_static()?;
        let file = HostFile {
            version: 1,
            signing_key: random_bytes(),
            static_private,
            static_public,
        };
        files::write_private(&path, &serde_json::to_vec_pretty(&file)?)?;
        Ok(Self {
            signing: SigningKey::from_bytes(&file.signing_key),
            static_private: Zeroizing::new(file.static_private),
            static_public: file.static_public,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn host_id(&self) -> HostId {
        host_id_for(&self.verifying_key())
    }

    pub fn verifying_key(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn static_public(&self) -> [u8; 32] {
        self.static_public
    }

    pub(crate) fn static_private(&self) -> &[u8; 32] {
        &self.static_private
    }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.signing.sign(message).to_bytes().to_vec()
    }
}

pub fn verify_signature(verifying_key: &[u8; 32], message: &[u8], signature: &[u8]) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(verifying_key) else {
        return false;
    };
    let Ok(signature) = ed25519_dalek::Signature::from_slice(signature) else {
        return false;
    };
    key.verify(message, &signature).is_ok()
}

pub struct ClientIdentity {
    static_private: Zeroizing<[u8; 32]>,
    static_public: [u8; 32],
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("public", &crate::encode_key(&self.static_public))
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    pub fn generate() -> Result<Self, CryptoError> {
        let (private, public) = generate_static()?;
        Ok(Self {
            static_private: Zeroizing::new(private),
            static_public: public,
        })
    }

    pub fn load_or_create(path: &Path) -> Result<Self, CryptoError> {
        let _lock = files::lock(path)?;
        if let Some(bytes) = files::read_optional(path)? {
            let file: ClientFile = serde_json::from_slice(&bytes)?;
            return Ok(Self {
                static_private: Zeroizing::new(file.static_private),
                static_public: file.static_public,
            });
        }
        let identity = Self::generate()?;
        let file = ClientFile {
            version: 1,
            static_private: *identity.static_private,
            static_public: identity.static_public,
        };
        files::write_private(path, &serde_json::to_vec_pretty(&file)?)?;
        Ok(identity)
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.static_public
    }

    pub(crate) fn static_private(&self) -> &[u8; 32] {
        &self.static_private
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn host_identity_persists_and_signs() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let first = HostIdentity::load_or_create(&data).unwrap();
        let second = HostIdentity::load_or_create(&data).unwrap();
        assert_eq!(first.host_id(), second.host_id());
        assert_eq!(first.static_public(), second.static_public());
        let mode = std::fs::metadata(first.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let signature = first.sign(b"hello");
        assert!(verify_signature(
            &first.verifying_key(),
            b"hello",
            &signature
        ));
        assert!(!verify_signature(
            &first.verifying_key(),
            b"other",
            &signature
        ));
        assert!(!verify_signature(
            &first.verifying_key(),
            b"hello",
            &[0; 10]
        ));
        assert_eq!(host_id_for(&first.verifying_key()), first.host_id());
    }

    #[test]
    fn client_identity_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client").join(CLIENT_IDENTITY_FILE);
        let first = ClientIdentity::load_or_create(&path).unwrap();
        let second = ClientIdentity::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        assert_ne!(
            ClientIdentity::generate().unwrap().public_key(),
            first.public_key()
        );
    }
}
