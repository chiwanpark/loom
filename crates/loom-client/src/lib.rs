mod client;
mod connect;
mod error;

pub use client::{Client, Notification, Notifications};
pub use connect::{
    AgentTarget, connect_local, connect_ssh, list_local_agents, list_remote_agents, local_registry,
};
pub use error::{ClientError, Result};
pub use loom_proto as proto;
pub use loom_transport::{AgentEntry, FrameStream, Registry, SshTarget, memory_pair};
