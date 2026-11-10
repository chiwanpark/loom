use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use loom_proto::InstanceId;
use loom_proto::relay::HostId;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::authorized::now_unix;
use crate::encoding::{decode_key, encode_key, key_b64};
use crate::error::CryptoError;
use crate::files;
use crate::identity::random_bytes;

pub const PAIRINGS_FILE: &str = "pairings.json";
pub const DEFAULT_PAIRING_TTL: Duration = Duration::from_secs(600);

pub fn pairing_id(secret: &[u8; 32]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"loom-pairing-id/1");
    hasher.update(secret);
    hasher.finalize().as_bytes()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingUri {
    pub relay_url: String,
    pub host: HostId,
    pub instance: InstanceId,
    pub host_static: [u8; 32],
    pub secret: Zeroizing<[u8; 32]>,
}

impl fmt::Debug for PairingUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingUri")
            .field("relay_url", &self.relay_url)
            .field("host", &self.host.to_string())
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for PairingUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (scheme, authority) = match self.relay_url.split_once("://") {
            Some(("ws", rest)) => ("loom+ws", rest),
            Some((_, rest)) => ("loom", rest),
            None => ("loom", self.relay_url.as_str()),
        };
        write!(
            f,
            "{scheme}://{}/{}/{}#{}.{}",
            authority.trim_end_matches('/'),
            self.host,
            self.instance,
            encode_key(&self.host_static),
            encode_key(&self.secret)
        )
    }
}

impl FromStr for PairingUri {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |what: &str| CryptoError::Invalid(format!("invalid pairing URI: {what}"));
        let s = s.trim();
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| invalid("missing scheme"))?;
        let ws_scheme = match scheme {
            "loom" => "wss",
            "loom+ws" => "ws",
            _ => return Err(invalid("the scheme must be loom://")),
        };
        let (path, fragment) = rest
            .split_once('#')
            .ok_or_else(|| invalid("missing keys"))?;
        let (static_key, secret) = fragment
            .split_once('.')
            .ok_or_else(|| invalid("malformed keys"))?;
        let mut parts = path.rsplitn(3, '/');
        let instance = parts.next().filter(|p| !p.is_empty());
        let host = parts.next();
        let authority = parts.next().filter(|p| !p.is_empty());
        let (Some(instance), Some(host), Some(authority)) = (instance, host, authority) else {
            return Err(invalid("expected loom://<relay>/<host>/<instance>"));
        };
        Ok(Self {
            relay_url: format!("{ws_scheme}://{authority}"),
            host: host.parse().map_err(|_| invalid("bad host ID"))?,
            instance: InstanceId(instance.to_owned()),
            host_static: decode_key(static_key).ok_or_else(|| invalid("bad host key"))?,
            secret: Zeroizing::new(decode_key(secret).ok_or_else(|| invalid("bad secret"))?),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Pending {
    id: String,
    #[serde(with = "key_b64")]
    secret: [u8; 32],
    expires_at: u64,
}

impl Drop for Pending {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.secret.zeroize();
    }
}

#[derive(Default, Serialize, Deserialize)]
struct PairingsFile {
    pending: Vec<Pending>,
}

#[derive(Debug, Clone)]
pub struct Pairings {
    path: PathBuf,
}

impl Pairings {
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(PAIRINGS_FILE),
        }
    }

    fn read(&self) -> Result<PairingsFile, CryptoError> {
        match files::read_optional(&self.path)? {
            Some(bytes) if !bytes.is_empty() => Ok(serde_json::from_slice(&bytes)?),
            _ => Ok(PairingsFile::default()),
        }
    }

    fn write(&self, file: &PairingsFile) -> Result<(), CryptoError> {
        files::write_private(&self.path, &serde_json::to_vec(file)?)?;
        Ok(())
    }

    pub fn create(&self, ttl: Duration) -> Result<(String, Zeroizing<[u8; 32]>), CryptoError> {
        let _lock = files::lock(&self.path)?;
        let mut file = self.read()?;
        let now = now_unix();
        file.pending.retain(|p| p.expires_at > now);
        let secret = Zeroizing::new(random_bytes());
        let id = pairing_id(&secret);
        file.pending.push(Pending {
            id: id.clone(),
            secret: *secret,
            expires_at: now + ttl.as_secs().max(1),
        });
        self.write(&file)?;
        Ok((id, secret))
    }

    pub fn take(&self, id: &str) -> Result<Option<Zeroizing<[u8; 32]>>, CryptoError> {
        let _lock = files::lock(&self.path)?;
        let mut file = self.read()?;
        let now = now_unix();
        let before = file.pending.len();
        let mut found = None;
        file.pending.retain(|p| {
            if p.id == id && p.expires_at > now {
                found = Some(Zeroizing::new(p.secret));
                false
            } else {
                p.expires_at > now
            }
        });
        if found.is_some() || file.pending.len() != before {
            self.write(&file)?;
        }
        Ok(found)
    }

    pub fn is_pending(&self, id: &str) -> Result<bool, CryptoError> {
        let now = now_unix();
        Ok(self
            .read()?
            .pending
            .iter()
            .any(|p| p.id == id && p.expires_at > now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let uri = PairingUri {
            relay_url: "wss://relay.example.com".into(),
            host: HostId([7; 32]),
            instance: InstanceId("abc".into()),
            host_static: [8; 32],
            secret: Zeroizing::new([9; 32]),
        };
        let text = uri.to_string();
        assert!(text.starts_with("loom://relay.example.com/0707"), "{text}");
        assert_eq!(text.parse::<PairingUri>().unwrap(), uri);
        let local = PairingUri {
            relay_url: "ws://127.0.0.1:9000".into(),
            ..uri.clone()
        };
        let text = local.to_string();
        assert!(text.starts_with("loom+ws://127.0.0.1:9000/"), "{text}");
        assert_eq!(text.parse::<PairingUri>().unwrap(), local);
        for bad in [
            "http://x/a/b#c.d",
            "loom://x/a#c.d",
            "loom://relay/zz/inst#a.b",
            "loom://relay/host/inst",
        ] {
            assert!(bad.parse::<PairingUri>().is_err(), "{bad}");
        }
    }

    #[test]
    fn secrets_are_single_use_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let pairings = Pairings::in_dir(dir.path());
        let (id, secret) = pairings.create(Duration::from_secs(60)).unwrap();
        assert_eq!(id, pairing_id(&secret));
        assert!(pairings.is_pending(&id).unwrap());
        assert_eq!(pairings.take(&id).unwrap().unwrap(), secret);
        assert!(pairings.take(&id).unwrap().is_none());
        assert!(!pairings.is_pending(&id).unwrap());
        assert!(pairings.take("unknown").unwrap().is_none());
    }
}
