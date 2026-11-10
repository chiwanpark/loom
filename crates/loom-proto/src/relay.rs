use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::api::InstanceId;

pub const RELAY_VERSION: u16 = 1;
pub type ChannelId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HostId(pub [u8; 32]);

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a host ID is 64 hex characters")]
pub struct InvalidHostId;

impl FromStr for HostId {
    type Err = InvalidHostId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.len() != 64 || !s.is_ascii() {
            return Err(InvalidHostId);
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| InvalidHostId)?;
        }
        Ok(Self(out))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub host: HostId,
    pub instance: InstanceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseReason {
    Requested,
    PeerGone,
    SlowConsumer,
    Replaced,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayErrorCode {
    BadRequest,
    Unauthorized,
    NotFound,
    RateLimited,
    LimitExceeded,
    VersionMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelayFrame {
    AgentHello {
        host_pubkey: [u8; 32],
        instance: InstanceId,
        version: u16,
    },
    AgentAuth {
        signature: Vec<u8>,
    },
    ListAgents {
        host: HostId,
    },
    Open {
        agent: AgentId,
    },
    Challenge {
        nonce: [u8; 32],
    },
    Registered {
        host: HostId,
    },
    Agents {
        host: HostId,
        instances: Vec<InstanceId>,
    },
    Opened {
        channel: ChannelId,
        agent: AgentId,
    },
    Close {
        channel: ChannelId,
    },
    Closed {
        channel: ChannelId,
        reason: CloseReason,
    },
    Presence {
        agent: AgentId,
        online: bool,
    },
    Data {
        channel: ChannelId,
        payload: Vec<u8>,
    },
    Error {
        code: RelayErrorCode,
        message: String,
    },
}

pub fn agent_auth_message(nonce: &[u8; 32], instance: &InstanceId) -> Vec<u8> {
    let mut message = b"loom-relay-agent-auth/1".to_vec();
    message.extend_from_slice(nonce);
    message.extend_from_slice(instance.0.as_bytes());
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn host_ids_round_trip_as_hex() {
        let id = HostId([0xab; 32]);
        let text = id.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<HostId>().unwrap(), id);
        assert!("zz".parse::<HostId>().is_err());
        assert!("g".repeat(64).parse::<HostId>().is_err());
    }

    #[test]
    fn frames_roundtrip_through_postcard() {
        let agent = AgentId {
            host: HostId([1; 32]),
            instance: InstanceId("i".into()),
        };
        for frame in [
            RelayFrame::AgentHello {
                host_pubkey: [2; 32],
                instance: InstanceId("i".into()),
                version: RELAY_VERSION,
            },
            RelayFrame::AgentAuth {
                signature: vec![3; 64],
            },
            RelayFrame::Open {
                agent: agent.clone(),
            },
            RelayFrame::Opened { channel: 9, agent },
            RelayFrame::Data {
                channel: 9,
                payload: vec![0; 70_000],
            },
            RelayFrame::Error {
                code: RelayErrorCode::RateLimited,
                message: "slow down".into(),
            },
        ] {
            let decoded: RelayFrame = decode(&encode(&frame).unwrap()).unwrap();
            assert_eq!(decoded, frame);
        }
    }
}
