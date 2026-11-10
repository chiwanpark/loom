use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::encoding::key_b64;
use crate::error::CryptoError;
use crate::files;

pub const AUTHORIZED_CLIENTS_FILE: &str = "authorized_clients.json";
const MAX_LABEL_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEntry {
    pub label: String,
    #[serde(with = "key_b64")]
    pub pubkey: [u8; 32],
    pub added_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ClientsFile {
    version: u32,
    clients: Vec<ClientEntry>,
}

#[derive(Debug, Clone)]
pub struct AuthorizedClients {
    path: PathBuf,
}

pub fn validate_label(label: &str) -> Result<(), CryptoError> {
    let ok = !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'));
    if ok {
        Ok(())
    } else {
        Err(CryptoError::Invalid(format!(
            "client labels are 1 to {MAX_LABEL_LEN} letters, digits, '-', '_', '.', or '@'"
        )))
    }
}

impl AuthorizedClients {
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(AUTHORIZED_CLIENTS_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<ClientsFile, CryptoError> {
        match files::read_optional(&self.path)? {
            Some(bytes) if !bytes.is_empty() => Ok(serde_json::from_slice(&bytes)?),
            _ => Ok(ClientsFile {
                version: 1,
                clients: Vec::new(),
            }),
        }
    }

    fn write(&self, file: &ClientsFile) -> Result<(), CryptoError> {
        files::write_private(&self.path, &serde_json::to_vec_pretty(file)?)?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<ClientEntry>, CryptoError> {
        Ok(self.read()?.clients)
    }

    pub fn find(&self, pubkey: &[u8; 32]) -> Result<Option<ClientEntry>, CryptoError> {
        Ok(self.list()?.into_iter().find(|c| &c.pubkey == pubkey))
    }

    pub fn authorize(&self, pubkey: [u8; 32], label: &str) -> Result<ClientEntry, CryptoError> {
        validate_label(label)?;
        let _lock = files::lock(&self.path)?;
        let mut file = self.read()?;
        file.clients
            .retain(|c| c.label != label && c.pubkey != pubkey);
        let entry = ClientEntry {
            label: label.to_owned(),
            pubkey,
            added_at: now_unix(),
        };
        file.clients.push(entry.clone());
        file.version = 1;
        self.write(&file)?;
        Ok(entry)
    }

    pub fn revoke(&self, label: &str) -> Result<bool, CryptoError> {
        let _lock = files::lock(&self.path)?;
        let mut file = self.read()?;
        let before = file.clients.len();
        file.clients.retain(|c| c.label != label);
        let removed = file.clients.len() != before;
        if removed {
            self.write(&file)?;
        }
        Ok(removed)
    }

    pub fn modified(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_list_and_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let clients = AuthorizedClients::in_dir(dir.path());
        assert!(clients.list().unwrap().is_empty());
        clients.authorize([1; 32], "laptop").unwrap();
        clients.authorize([2; 32], "phone").unwrap();
        assert_eq!(clients.find(&[1; 32]).unwrap().unwrap().label, "laptop");
        clients.authorize([3; 32], "laptop").unwrap();
        assert!(clients.find(&[1; 32]).unwrap().is_none());
        let labels: Vec<_> = clients
            .list()
            .unwrap()
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(labels, ["phone", "laptop"]);
        assert!(clients.revoke("phone").unwrap());
        assert!(!clients.revoke("phone").unwrap());
        assert_eq!(clients.list().unwrap().len(), 1);
        assert!(clients.authorize([4; 32], "bad label").is_err());
        assert!(clients.authorize([4; 32], "").is_err());
    }
}
