mod client;
mod close_guard;
mod connect;
mod download;
mod error;
mod forward;
mod login;
mod oracle;
mod relay;
mod update;
mod upload;

pub use client::{Client, Notification, Notifications, terminal_owner};
pub use close_guard::{CloseChoice, CloseGuardError, choices_from_lists, close_plan, needs_choice};
pub use connect::{
    AgentTarget, connect_local, connect_ssh, list_local_agents, list_remote_agents, local_registry,
};
pub use error::{ClientError, Result};
pub use forward::{PortForward, Tunnel};
pub use login::BrowserCallback;
pub use loom_crypto::ClientIdentity;
pub use loom_oracle_proto as oracle_proto;
pub use loom_proto as proto;
pub use loom_transport::{
    AccessToken, AgentEntry, FrameStream, Registry, RelayError, SshTarget, memory_pair,
};
pub use oracle::{
    NewCodespace, OracleCalls, OracleClient, OracleEndpoint, OracleEvents, pair_oracle,
};
pub use relay::{RelayHost, client_identity_path, load_client_identity, pair_with_uri};
pub use update::{agent_protocol, can_update_agent, update_agent_of_protocol, update_agent_with};
