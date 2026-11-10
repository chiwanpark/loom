use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use loom_config::RelayServerConfig;
use loom_crypto::{host_id_for, verify_signature};
use loom_proto::relay::{
    AgentId, ChannelId, CloseReason, HostId, RELAY_VERSION, RelayErrorCode, RelayFrame,
    agent_auth_message,
};
use loom_proto::{InstanceId, decode, encode};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::access::{AccessVerifier, JWT_HEADER};
use crate::limits::IpLimits;

const HEARTBEAT: Duration = Duration::from_secs(30);
const CONNECTING_IP: &str = "cf-connecting-ip";

type ConnId = u64;

#[derive(Clone)]
struct Peer {
    conn: ConnId,
    tx: mpsc::Sender<RelayFrame>,
    kill: CancellationToken,
    stats: Arc<Stats>,
}

impl Peer {
    fn send(&self, frame: RelayFrame) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(frame) {
            tracing::info!(conn = self.conn, "disconnecting a slow consumer");
            bump(&self.stats.slow_consumers, 1);
            self.kill.cancel();
        }
    }
}

struct AgentSlot {
    peer: Peer,
    channels: usize,
}

struct Channel {
    agent: AgentId,
    agent_peer: Peer,
    client: Peer,
}

#[derive(Default)]
struct Routes {
    agents: HashMap<HostId, HashMap<InstanceId, AgentSlot>>,
    channels: HashMap<ChannelId, Channel>,
    watchers: HashMap<HostId, HashMap<ConnId, Peer>>,
}

#[derive(Default)]
struct Stats {
    connections_total: AtomicU64,
    agents_registered_total: AtomicU64,
    channels_opened_total: AtomicU64,
    frames_forwarded: AtomicU64,
    bytes_forwarded: AtomicU64,
    rejected_access: AtomicU64,
    rejected_hosts: AtomicU64,
    rate_limited: AtomicU64,
    slow_consumers: AtomicU64,
    client_connections: AtomicU64,
}

fn bump(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

pub struct Relay {
    config: RelayServerConfig,
    allowed: HashSet<HostId>,
    access: Option<AccessVerifier>,
    limits: IpLimits,
    routes: Mutex<Routes>,
    next_id: AtomicU64,
    shutdown: CancellationToken,
    stats: Arc<Stats>,
}

#[derive(Debug, Clone, Copy)]
enum Role {
    Agent,
    Client,
}

impl Relay {
    pub fn new(config: RelayServerConfig) -> Arc<Self> {
        let allowed = config
            .allowed_hosts
            .iter()
            .filter_map(|h| h.parse().ok())
            .collect();
        let access = config.cloudflare_access.clone().map(AccessVerifier::new);
        let limits = IpLimits::new(
            config.limits.messages_per_second,
            config.limits.burst,
            config.limits.max_connections_per_ip,
        );
        Arc::new(Self {
            config,
            allowed,
            access,
            limits,
            routes: Mutex::new(Routes::default()),
            next_id: AtomicU64::new(1),
            shutdown: CancellationToken::new(),
            stats: Arc::new(Stats::default()),
        })
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/v1/agent", get(agent_upgrade))
            .route("/v1/client", get(client_upgrade))
            .with_state(self.clone())
    }

    pub fn metrics_router(self: &Arc<Self>) -> Router {
        let relay = self.clone();
        Router::new().route(
            "/metrics",
            get(move || {
                let relay = relay.clone();
                async move { relay.metrics_text() }
            }),
        )
    }

    pub fn metrics_text(&self) -> String {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let s = &self.stats;
        let rows: [(&str, &str, &str, u64); 12] = [
            (
                "loom_relay_agents_online",
                "gauge",
                "Registered agents",
                self.online_agents() as u64,
            ),
            (
                "loom_relay_clients_connected",
                "gauge",
                "Connected clients",
                get(&s.client_connections),
            ),
            (
                "loom_relay_channels_open",
                "gauge",
                "Open channels",
                self.open_channels() as u64,
            ),
            (
                "loom_relay_connections_total",
                "counter",
                "Accepted WebSocket connections",
                get(&s.connections_total),
            ),
            (
                "loom_relay_agents_registered_total",
                "counter",
                "Agent registrations",
                get(&s.agents_registered_total),
            ),
            (
                "loom_relay_channels_opened_total",
                "counter",
                "Opened channels",
                get(&s.channels_opened_total),
            ),
            (
                "loom_relay_frames_forwarded_total",
                "counter",
                "Forwarded data frames",
                get(&s.frames_forwarded),
            ),
            (
                "loom_relay_bytes_forwarded_total",
                "counter",
                "Forwarded payload bytes",
                get(&s.bytes_forwarded),
            ),
            (
                "loom_relay_rejected_access_total",
                "counter",
                "Requests without a valid Access token",
                get(&s.rejected_access),
            ),
            (
                "loom_relay_rejected_hosts_total",
                "counter",
                "Agents rejected by the allow-list or signature",
                get(&s.rejected_hosts),
            ),
            (
                "loom_relay_rate_limited_total",
                "counter",
                "Connections closed by rate limits",
                get(&s.rate_limited),
            ),
            (
                "loom_relay_slow_consumers_total",
                "counter",
                "Connections closed for falling behind",
                get(&s.slow_consumers),
            ),
        ];
        let mut out = String::new();
        for (name, kind, help, value) in rows {
            out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
            ));
        }
        out
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    fn routes(&self) -> MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn online_agents(&self) -> usize {
        self.routes().agents.values().map(HashMap::len).sum()
    }

    pub fn open_channels(&self) -> usize {
        self.routes().channels.len()
    }

    fn client_ip(&self, headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
        if self.access.is_some()
            && let Some(ip) = headers
                .get(CONNECTING_IP)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
        addr.ip()
    }
}

#[derive(Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

async fn agent_upgrade(
    state: State<Arc<Relay>>,
    addr: ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(state.0, addr.0.0, headers, ws, Role::Agent).await
}

async fn client_upgrade(
    state: State<Arc<Relay>>,
    addr: ConnectInfo<PeerAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    upgrade(state.0, addr.0.0, headers, ws, Role::Client).await
}

async fn upgrade(
    relay: Arc<Relay>,
    addr: SocketAddr,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    role: Role,
) -> Response {
    if let Some(access) = &relay.access {
        let token = headers.get(JWT_HEADER).and_then(|v| v.to_str().ok());
        let Some(token) = token else {
            bump(&relay.stats.rejected_access, 1);
            return (StatusCode::FORBIDDEN, "missing Access token").into_response();
        };
        if let Err(err) = access.verify(token).await {
            bump(&relay.stats.rejected_access, 1);
            tracing::info!(%addr, error = %err, "rejected a request without a valid Access token");
            return (StatusCode::FORBIDDEN, "invalid Access token").into_response();
        }
    }
    let ip = relay.client_ip(&headers, addr);
    if !relay.limits.connect(ip) {
        bump(&relay.stats.rate_limited, 1);
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }
    let max = relay.config.limits.max_frame.as_usize();
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| async move {
            run_connection(relay.clone(), socket, ip, role).await;
            relay.limits.disconnect(ip);
        })
}

struct Conn {
    relay: Arc<Relay>,
    peer: Peer,
    ip: IpAddr,
    registered: Option<AgentId>,
    watching: HashSet<HostId>,
}

async fn run_connection(relay: Arc<Relay>, socket: WebSocket, ip: IpAddr, role: Role) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<RelayFrame>(relay.config.limits.queue_frames);
    let kill = relay.shutdown.child_token();
    let conn_id = relay.next_id();
    bump(&relay.stats.connections_total, 1);
    if matches!(role, Role::Client) {
        bump(&relay.stats.client_connections, 1);
    }
    let writer_kill = kill.clone();
    let writer = tokio::spawn(async move {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        loop {
            tokio::select! {
                _ = writer_kill.cancelled() => break,
                frame = rx.recv() => {
                    let Some(frame) = frame else { break };
                    let Ok(bytes) = encode(&frame) else { break };
                    if let Err(err) = sink.send(Message::Binary(bytes.into())).await {
                        tracing::debug!(conn = conn_id, error = %err, "write failed");
                        break;
                    }
                }
                _ = heartbeat.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
        while let Ok(frame) = rx.try_recv() {
            if let Ok(bytes) = encode(&frame)
                && sink.send(Message::Binary(bytes.into())).await.is_err()
            {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });
    let mut conn = Conn {
        relay: relay.clone(),
        peer: Peer {
            conn: conn_id,
            tx,
            kill: kill.clone(),
            stats: relay.stats.clone(),
        },
        ip,
        registered: None,
        watching: HashSet::new(),
    };
    tracing::debug!(conn = conn_id, ?role, %ip, "connection opened");
    let result = match role {
        Role::Agent => conn.run_agent(&mut stream).await,
        Role::Client => conn.run_client(&mut stream).await,
    };
    if let Err((code, message)) = result {
        tracing::debug!(conn = conn_id, ?code, %message, "closing a connection");
        let _ = conn.peer.tx.try_send(RelayFrame::Error { code, message });
    }
    conn.cleanup();
    drop(conn);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    kill.cancel();
    if matches!(role, Role::Client) {
        relay
            .stats
            .client_connections
            .fetch_sub(1, Ordering::Relaxed);
    }
}

type Failure = (RelayErrorCode, String);

fn bad(message: impl Into<String>) -> Failure {
    (RelayErrorCode::BadRequest, message.into())
}

impl Conn {
    async fn read(
        &self,
        stream: &mut futures::stream::SplitStream<WebSocket>,
        timeout: Duration,
    ) -> Result<Option<RelayFrame>, Failure> {
        loop {
            let message = tokio::select! {
                _ = self.peer.kill.cancelled() => {
                    tracing::debug!(conn = self.peer.conn, "connection killed");
                    return Ok(None);
                }
                message = tokio::time::timeout(timeout, stream.next()) => message,
            };
            let message = match message {
                Err(_) => return Err(bad("the connection timed out")),
                Ok(None) | Ok(Some(Ok(Message::Close(_)))) => return Ok(None),
                Ok(Some(Err(err))) => {
                    tracing::debug!(conn = self.peer.conn, error = %err, "read failed");
                    return Ok(None);
                }
                Ok(Some(Ok(message))) => message,
            };
            let Message::Binary(bytes) = message else {
                continue;
            };
            if !self.relay.limits.allow_message(self.ip) {
                bump(&self.relay.stats.rate_limited, 1);
                return Err((
                    RelayErrorCode::RateLimited,
                    "too many messages; slow down".into(),
                ));
            }
            return decode::<RelayFrame>(&bytes)
                .map(Some)
                .map_err(|_| bad("malformed frame"));
        }
    }

    async fn run_agent(
        &mut self,
        stream: &mut futures::stream::SplitStream<WebSocket>,
    ) -> Result<(), Failure> {
        let limits = &self.relay.config.limits;
        let handshake = limits.handshake_timeout.as_duration();
        let idle = limits.idle_timeout.as_duration();
        let Some(RelayFrame::AgentHello {
            host_pubkey,
            instance,
            version,
        }) = self.read(stream, handshake).await?
        else {
            return Err(bad("expected AgentHello"));
        };
        if version != RELAY_VERSION {
            return Err((
                RelayErrorCode::VersionMismatch,
                format!("the relay speaks version {RELAY_VERSION}, not {version}"),
            ));
        }
        if instance.0.is_empty() || instance.0.len() > 128 {
            return Err(bad("invalid instance ID"));
        }
        let host = host_id_for(&host_pubkey);
        if !self.relay.allowed.contains(&host) {
            bump(&self.relay.stats.rejected_hosts, 1);
            tracing::info!(%host, "rejected an agent whose host is not allowed");
            return Err((
                RelayErrorCode::Unauthorized,
                format!("host {host} is not in allowed_hosts"),
            ));
        }
        let mut nonce = [0u8; 32];
        rand::fill(&mut nonce);
        self.peer.send(RelayFrame::Challenge { nonce });
        let Some(RelayFrame::AgentAuth { signature }) = self.read(stream, handshake).await? else {
            return Err(bad("expected AgentAuth"));
        };
        if !verify_signature(
            &host_pubkey,
            &agent_auth_message(&nonce, &instance),
            &signature,
        ) {
            bump(&self.relay.stats.rejected_hosts, 1);
            return Err((RelayErrorCode::Unauthorized, "bad signature".into()));
        }
        let agent = AgentId { host, instance };
        self.register(agent.clone())?;
        bump(&self.relay.stats.agents_registered_total, 1);
        self.peer.send(RelayFrame::Registered { host });
        tracing::info!(%host, instance = %agent.instance, "agent registered");
        while let Some(frame) = self.read(stream, idle).await? {
            match frame {
                RelayFrame::Data { channel, payload } => {
                    self.forward(channel, payload, Role::Agent)
                }
                RelayFrame::Close { channel } => self.close_channel(channel, Role::Agent),
                _ => return Err(bad("unexpected frame from an agent")),
            }
        }
        Ok(())
    }

    fn register(&mut self, agent: AgentId) -> Result<(), Failure> {
        let max = self.relay.config.limits.max_agents_per_host;
        let (replaced, watchers) = {
            let mut routes = self.relay.routes();
            let slots = routes.agents.entry(agent.host).or_default();
            if !slots.contains_key(&agent.instance) && slots.len() >= max {
                return Err((
                    RelayErrorCode::LimitExceeded,
                    "too many agents for this host".into(),
                ));
            }
            let replaced = slots.insert(
                agent.instance.clone(),
                AgentSlot {
                    peer: self.peer.clone(),
                    channels: 0,
                },
            );
            let watchers: Vec<Peer> = routes
                .watchers
                .get(&agent.host)
                .map(|w| w.values().cloned().collect())
                .unwrap_or_default();
            (replaced, watchers)
        };
        if let Some(old) = replaced {
            old.peer.kill.cancel();
        }
        for watcher in watchers {
            watcher.send(RelayFrame::Presence {
                agent: agent.clone(),
                online: true,
            });
        }
        self.registered = Some(agent);
        Ok(())
    }

    async fn run_client(
        &mut self,
        stream: &mut futures::stream::SplitStream<WebSocket>,
    ) -> Result<(), Failure> {
        let idle = self.relay.config.limits.idle_timeout.as_duration();
        while let Some(frame) = self.read(stream, idle).await? {
            match frame {
                RelayFrame::ListAgents { host } => {
                    let instances = {
                        let mut routes = self.relay.routes();
                        routes
                            .watchers
                            .entry(host)
                            .or_default()
                            .insert(self.peer.conn, self.peer.clone());
                        let mut instances: Vec<InstanceId> = routes
                            .agents
                            .get(&host)
                            .map(|slots| slots.keys().cloned().collect())
                            .unwrap_or_default();
                        instances.sort();
                        instances
                    };
                    self.watching.insert(host);
                    self.peer.send(RelayFrame::Agents { host, instances });
                }
                RelayFrame::Open { agent } => self.open(agent),
                RelayFrame::Data { channel, payload } => {
                    self.forward(channel, payload, Role::Client)
                }
                RelayFrame::Close { channel } => self.close_channel(channel, Role::Client),
                _ => return Err(bad("unexpected frame from a client")),
            }
        }
        Ok(())
    }

    fn open(&mut self, agent: AgentId) {
        let max = self.relay.config.limits.max_channels_per_agent;
        let channel = self.relay.next_id();
        let result = {
            let mut routes = self.relay.routes();
            let own = routes
                .channels
                .values()
                .filter(|c| c.client.conn == self.peer.conn)
                .count();
            match routes
                .agents
                .get_mut(&agent.host)
                .and_then(|slots| slots.get_mut(&agent.instance))
            {
                None => Err((RelayErrorCode::NotFound, "the agent is offline".to_owned())),
                Some(slot) if slot.channels >= max || own >= max => Err((
                    RelayErrorCode::LimitExceeded,
                    "too many channels".to_owned(),
                )),
                Some(slot) => {
                    slot.channels += 1;
                    let agent_peer = slot.peer.clone();
                    routes.channels.insert(
                        channel,
                        Channel {
                            agent: agent.clone(),
                            agent_peer: agent_peer.clone(),
                            client: self.peer.clone(),
                        },
                    );
                    Ok(agent_peer)
                }
            }
        };
        match result {
            Ok(agent_peer) => {
                bump(&self.relay.stats.channels_opened_total, 1);
                agent_peer.send(RelayFrame::Opened {
                    channel,
                    agent: agent.clone(),
                });
                self.peer.send(RelayFrame::Opened { channel, agent });
            }
            Err((code, message)) => self.peer.send(RelayFrame::Error { code, message }),
        }
    }

    fn forward(&self, channel: ChannelId, payload: Vec<u8>, from: Role) {
        let target = {
            let routes = self.relay.routes();
            routes.channels.get(&channel).and_then(|c| match from {
                Role::Agent if c.agent_peer.conn == self.peer.conn => Some(c.client.clone()),
                Role::Client if c.client.conn == self.peer.conn => Some(c.agent_peer.clone()),
                _ => None,
            })
        };
        if let Some(target) = target {
            bump(&self.relay.stats.frames_forwarded, 1);
            bump(&self.relay.stats.bytes_forwarded, payload.len() as u64);
            target.send(RelayFrame::Data { channel, payload });
        }
    }

    fn close_channel(&self, channel: ChannelId, from: Role) {
        let removed = {
            let mut routes = self.relay.routes();
            let owned = routes.channels.get(&channel).is_some_and(|c| match from {
                Role::Agent => c.agent_peer.conn == self.peer.conn,
                Role::Client => c.client.conn == self.peer.conn,
            });
            if owned {
                remove_channel(&mut routes, channel)
            } else {
                None
            }
        };
        if let Some(c) = removed {
            let other = match from {
                Role::Agent => c.client,
                Role::Client => c.agent_peer,
            };
            other.send(RelayFrame::Closed {
                channel,
                reason: CloseReason::Requested,
            });
        }
    }

    fn cleanup(&mut self) {
        let mut notify = Vec::new();
        {
            let mut routes = self.relay.routes();
            let own: Vec<ChannelId> = routes
                .channels
                .iter()
                .filter(|(_, c)| {
                    c.client.conn == self.peer.conn || c.agent_peer.conn == self.peer.conn
                })
                .map(|(id, _)| *id)
                .collect();
            for id in own {
                if let Some(c) = remove_channel(&mut routes, id) {
                    let other = if c.client.conn == self.peer.conn {
                        c.agent_peer
                    } else {
                        c.client
                    };
                    notify.push((
                        other,
                        RelayFrame::Closed {
                            channel: id,
                            reason: CloseReason::PeerGone,
                        },
                    ));
                }
            }
            for host in &self.watching {
                if let Some(watchers) = routes.watchers.get_mut(host) {
                    watchers.remove(&self.peer.conn);
                    if watchers.is_empty() {
                        routes.watchers.remove(host);
                    }
                }
            }
            if let Some(agent) = self.registered.take() {
                let mine = routes
                    .agents
                    .get(&agent.host)
                    .and_then(|slots| slots.get(&agent.instance))
                    .is_some_and(|slot| slot.peer.conn == self.peer.conn);
                if mine {
                    if let Some(slots) = routes.agents.get_mut(&agent.host) {
                        slots.remove(&agent.instance);
                        if slots.is_empty() {
                            routes.agents.remove(&agent.host);
                        }
                    }
                    for watcher in routes.watchers.get(&agent.host).into_iter().flatten() {
                        notify.push((
                            watcher.1.clone(),
                            RelayFrame::Presence {
                                agent: agent.clone(),
                                online: false,
                            },
                        ));
                    }
                    tracing::info!(host = %agent.host, instance = %agent.instance, "agent went offline");
                }
            }
        }
        for (peer, frame) in notify {
            peer.send(frame);
        }
    }
}

fn remove_channel(routes: &mut Routes, id: ChannelId) -> Option<Channel> {
    let channel = routes.channels.remove(&id)?;
    if let Some(slot) = routes
        .agents
        .get_mut(&channel.agent.host)
        .and_then(|slots| slots.get_mut(&channel.agent.instance))
        && slot.peer.conn == channel.agent_peer.conn
    {
        slot.channels = slot.channels.saturating_sub(1);
    }
    Some(channel)
}
