use std::path::Path;

use zeroize::Zeroizing;

use crate::authorized::AuthorizedClients;
use crate::noise::Responder;
use crate::pairing::Pairings;

#[derive(Debug, Clone)]
pub struct FileResponder {
    pub clients: AuthorizedClients,
    pub pairings: Pairings,
}

impl FileResponder {
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            clients: AuthorizedClients::in_dir(data_dir),
            pairings: Pairings::in_dir(data_dir),
        }
    }
}

impl Responder for FileResponder {
    fn authorize(&self, client: &[u8; 32]) -> Result<String, String> {
        match self.clients.find(client) {
            Ok(Some(entry)) => Ok(entry.label),
            Ok(None) => Err("this client is not authorized; pair it first".into()),
            Err(err) => {
                tracing::error!(error = %err, "cannot read the authorized client list");
                Err("the agent cannot read its authorized client list".into())
            }
        }
    }

    fn take_pairing(&self, id: &str) -> Option<Zeroizing<[u8; 32]>> {
        self.pairings.take(id).unwrap_or_else(|err| {
            tracing::error!(error = %err, "cannot read pending pairings");
            None
        })
    }

    fn paired(&self, client: [u8; 32], label: &str) -> Result<(), String> {
        self.clients
            .authorize(client, label)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}
