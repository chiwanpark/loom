use std::path::PathBuf;

use loom_config::Dirs;
use loom_crypto::{CLIENT_IDENTITY_FILE, ClientIdentity, decode_key, encode_key};
use loom_proto::InstanceId;
use loom_proto::relay::{AgentId, HostId};
use loom_transport::{AccessToken, FrameStream, RelayClient, RelayEndpoint};

use crate::error::{ClientError, Result};

pub fn client_identity_path(dirs: &Dirs) -> PathBuf {
    dirs.default_data_dir().join(CLIENT_IDENTITY_FILE)
}

pub fn load_client_identity(dirs: &Dirs) -> Result<ClientIdentity> {
    Ok(ClientIdentity::load_or_create(&client_identity_path(dirs))?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayHost {
    pub url: String,
    pub host: HostId,
    pub host_key: [u8; 32],
    pub access: Option<AccessToken>,
}

impl RelayHost {
    pub fn from_parts(
        url: &str,
        host_id: &str,
        host_key: &str,
        access: Option<AccessToken>,
    ) -> Result<Self> {
        let host = host_id
            .parse()
            .map_err(|_| ClientError::Protocol(format!("invalid host ID {host_id:?}")))?;
        let host_key =
            decode_key(host_key).ok_or_else(|| ClientError::Protocol("invalid host key".into()))?;
        Ok(Self {
            url: url.to_owned(),
            host,
            host_key,
            access,
        })
    }

    pub fn host_key_text(&self) -> String {
        encode_key(&self.host_key)
    }

    fn endpoint(&self) -> RelayEndpoint {
        RelayEndpoint {
            url: self.url.clone(),
            access: self.access.clone(),
        }
    }

    pub async fn list_agents(&self) -> Result<Vec<InstanceId>> {
        let mut client = RelayClient::connect(&self.endpoint()).await?;
        Ok(client.list_agents(self.host).await?)
    }

    pub async fn connect(
        &self,
        instance: Option<&InstanceId>,
        identity: &ClientIdentity,
    ) -> Result<FrameStream> {
        let instance = match instance {
            Some(instance) => instance.clone(),
            None => self
                .list_agents()
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| ClientError::NoAgent(format!("host {}", self.host)))?,
        };
        let agent = AgentId {
            host: self.host,
            instance,
        };
        Ok(
            loom_transport::connect_relay(&self.endpoint(), agent, identity, &self.host_key)
                .await?,
        )
    }
}
