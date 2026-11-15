//! Ports on the agent's host, and local ports of the phone forwarded to
//! them, so the phone's browser reaches a server the agent's host runs.

use std::sync::{Arc, Mutex};

use loom_client::PortForward;
use loom_client::proto::ListeningPort;

use crate::agent::{AgentConnection, with_client};
use crate::error::{LoomError, Result};
use crate::runtime::run;

/// A TCP port that a program listens on, on the agent's host.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HostPort {
    pub port: u16,
    /// The address it listens on, such as `127.0.0.1` or `::`.
    pub address: String,
    /// The host a forward connects to for it.
    pub host: String,
    pub process: Option<String>,
    pub pid: Option<u32>,
}

impl From<ListeningPort> for HostPort {
    fn from(port: ListeningPort) -> Self {
        Self {
            host: port.connect_host(),
            port: port.port,
            address: port.address,
            process: port.process,
            pid: port.pid,
        }
    }
}

/// A local port of the phone that forwards each connection to a port on
/// the agent's host, through the link to the agent, also after it
/// reconnects. It stops when `stop` is called or it is dropped.
#[derive(uniffi::Object)]
pub struct Forward {
    host: String,
    port: u16,
    local: u16,
    inner: Mutex<Option<PortForward>>,
}

#[uniffi::export]
impl Forward {
    /// The host and port on the agent's side.
    pub fn host(&self) -> String {
        self.host.clone()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The port on the phone's loopback address.
    pub fn local_port(&self) -> u16 {
        self.local
    }

    /// How many connections it carries now.
    pub fn connections(&self) -> u32 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |forward| forward.connections() as u32)
    }

    /// Stops listening and closes its connections.
    pub fn stop(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

#[uniffi::export]
impl AgentConnection {
    /// The TCP ports that programs of the agent's user listen on.
    pub async fn list_ports(self: Arc<Self>) -> Result<Vec<HostPort>> {
        with_client(&self.shared, move |client| async move {
            Ok(client
                .list_ports()
                .await?
                .into_iter()
                .map(HostPort::from)
                .collect())
        })
        .await
    }

    /// Forwards a local port to `host:port` on the agent's host: `local`
    /// when given, as to keep the port of an earlier forward; otherwise
    /// `port` itself when it is free on the phone, else any free port.
    pub async fn forward_port(
        self: Arc<Self>,
        host: String,
        port: u16,
        local: Option<u16>,
    ) -> Result<Arc<Forward>> {
        let shared = self.shared.clone();
        let forward_host = host.clone();
        let forward = run(async move {
            PortForward::start(local, forward_host, port, move || {
                shared.client().ok().filter(|client| !client.is_closed())
            })
            .await
        })
        .await
        .map_err(|err| LoomError::invalid(format!("could not listen on a local port: {err}")))?;
        Ok(Arc::new(Forward {
            host,
            port,
            local: forward.local_addr().port(),
            inner: Mutex::new(Some(forward)),
        }))
    }
}
