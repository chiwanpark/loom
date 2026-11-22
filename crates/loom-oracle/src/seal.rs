use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use loom_crypto::{Zeroizing, decode_key, encode_key};

const SEAL_VERSION: u8 = 1;
const NONCE_LEN: usize = 24;
const CHECK_CONTEXT: &str = "loom-oracle 2026-11 master key check";
const SEAL_CONTEXT: &str = "loom-oracle 2026-11 secrets";

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("cannot read the master key {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("cannot write the master key {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("the master key {0} is malformed; it is 43 base64url characters")]
    Malformed(String),
    #[error("cannot seal a secret")]
    Seal,
    #[error("cannot open a sealed secret: the master key is wrong or the data is damaged")]
    Open,
}

pub struct MasterKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKey").finish_non_exhaustive()
    }
}

impl MasterKey {
    pub fn generate() -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        rand::fill(&mut *key);
        Self(key)
    }

    pub fn load(path: &Path) -> Result<Option<Self>, SealError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => Zeroizing::new(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(SealError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        decode_key(&text)
            .map(|key| Some(Self(Zeroizing::new(key))))
            .ok_or_else(|| SealError::Malformed(path.display().to_string()))
    }

    pub fn create(path: &Path) -> Result<Self, SealError> {
        let failed = |source| SealError::Write {
            path: path.display().to_string(),
            source,
        };
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(failed)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .map_err(failed)?;
        }
        let key = Self::generate();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(failed)?;
        let text = Zeroizing::new(format!("{}\n", encode_key(&key.0)));
        file.write_all(text.as_bytes()).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        Ok(key)
    }

    pub fn check(&self) -> [u8; 32] {
        blake3::derive_key(CHECK_CONTEXT, &*self.0)
    }

    pub fn sealer(&self) -> Sealer {
        let key = Zeroizing::new(blake3::derive_key(SEAL_CONTEXT, &*self.0));
        Sealer {
            cipher: XChaCha20Poly1305::new(Key::from_slice(&*key)),
        }
    }
}

pub struct Sealer {
    cipher: XChaCha20Poly1305,
}

impl Sealer {
    pub fn seal(&self, context: &str, plain: &[u8]) -> Result<Vec<u8>, SealError> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let sealed = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plain,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| SealError::Seal)?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(SEAL_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    pub fn open(&self, context: &str, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
        let Some((&SEAL_VERSION, rest)) = sealed.split_first() else {
            return Err(SealError::Open);
        };
        if rest.len() < NONCE_LEN {
            return Err(SealError::Open);
        }
        let (nonce, body) = rest.split_at(NONCE_LEN);
        self.cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: context.as_bytes(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| SealError::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_secrets_open_only_with_their_key_and_context() {
        let key = MasterKey::generate();
        let sealer = key.sealer();
        let sealed = sealer.seal("pairings/a", b"secret").unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        assert_eq!(&**sealer.open("pairings/a", &sealed).unwrap(), b"secret");
        assert_ne!(sealer.seal("pairings/a", b"secret").unwrap(), sealed);
        assert!(matches!(
            sealer.open("pairings/b", &sealed),
            Err(SealError::Open)
        ));
        let other = MasterKey::generate().sealer();
        assert!(other.open("pairings/a", &sealed).is_err());
        let mut damaged = sealed.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(sealer.open("pairings/a", &damaged).is_err());
        assert!(sealer.open("pairings/a", &[SEAL_VERSION, 1, 2]).is_err());
        assert!(sealer.open("pairings/a", &[]).is_err());
    }

    #[test]
    fn key_files_are_private_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/master.key");
        assert!(MasterKey::load(&path).unwrap().is_none());
        let key = MasterKey::create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let loaded = MasterKey::load(&path).unwrap().unwrap();
        assert_eq!(loaded.check(), key.check());
        assert!(MasterKey::create(&path).is_err());
        std::fs::write(&path, "short\n").unwrap();
        assert!(matches!(
            MasterKey::load(&path),
            Err(SealError::Malformed(_))
        ));
    }
}
