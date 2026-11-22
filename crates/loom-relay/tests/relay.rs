use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use loom_config::RelayServerConfig;
use loom_crypto::{ClientIdentity, FileResponder, HostIdentity, PairingUri, Pairings, Zeroizing};
use loom_proto::InstanceId;
use loom_proto::relay::{AgentId, HostId};
use loom_relay::{HostCheck, Relay, serve};
use loom_transport::{
    AgentLinkOptions, IncomingChannel, LinkState, LocalLink, RelayClient, RelayEndpoint,
    RelayError, connect_relay, pair_relay, serve_local_link, spawn_agent_link,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

struct TestRelay {
    url: String,
    relay: Arc<Relay>,
    shutdown: CancellationToken,
}

async fn start_relay(config: RelayServerConfig) -> TestRelay {
    start_relay_with(Relay::new(config)).await
}

async fn start_relay_with_check(config: RelayServerConfig, hosts: Arc<dyn HostCheck>) -> TestRelay {
    start_relay_with(Relay::with_host_check(config, hosts)).await
}

async fn start_relay_with(relay: Arc<Relay>) -> TestRelay {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(serve(relay.clone(), listener, shutdown.clone()));
    TestRelay {
        url: format!("ws://{addr}"),
        relay,
        shutdown,
    }
}

struct TestAgent {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    instance: InstanceId,
    state: watch::Receiver<LinkState>,
    shutdown: CancellationToken,
}

fn host_in(dir: &std::path::Path) -> Arc<HostIdentity> {
    Arc::new(HostIdentity::load_or_create(dir).unwrap())
}

fn start_agent(
    url: &str,
    dir: tempfile::TempDir,
    host: Arc<HostIdentity>,
    name: &str,
) -> TestAgent {
    let data = dir.path().to_path_buf();
    let instance = InstanceId(name.into());
    let (tx, mut rx) = mpsc::channel::<IncomingChannel>(8);
    let shutdown = CancellationToken::new();
    let (_task, state) = spawn_agent_link(
        AgentLinkOptions {
            endpoint: RelayEndpoint::new(url),
            instance: instance.clone(),
            host: host.clone(),
            responder: Arc::new(FileResponder::in_dir(&data)),
            push: None,
        },
        tx,
        shutdown.clone(),
    );
    tokio::spawn(async move {
        while let Some(incoming) = rx.recv().await {
            tokio::spawn(async move {
                let mut stream = incoming.stream;
                let label = incoming.peer.label.clone();
                while let Ok(Some(frame)) = stream.recv().await {
                    let mut reply = label.as_bytes().to_vec();
                    reply.push(b':');
                    reply.extend_from_slice(&frame);
                    if stream.send(Bytes::from(reply)).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    TestAgent {
        _dir: dir,
        data,
        instance,
        state,
        shutdown,
    }
}

async fn wait_state(
    state: &mut watch::Receiver<LinkState>,
    check: impl Fn(&LinkState) -> bool,
) -> LinkState {
    tokio::time::timeout(Duration::from_secs(10), state.wait_for(|s| check(s)))
        .await
        .expect("timed out waiting for the link")
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn pairs_and_relays_encrypted_frames() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let config = RelayServerConfig {
        allowed_hosts: vec![host.host_id().to_string()],
        ..Default::default()
    };
    let relay = start_relay(config).await;
    let mut agent = start_agent(&relay.url, dir, host.clone(), "inst-1");
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    assert_eq!(relay.relay.online_agents(), 1);

    let endpoint = RelayEndpoint::new(&relay.url);
    let mut client = RelayClient::connect(&endpoint).await.unwrap();
    assert_eq!(
        client.list_agents(host.host_id()).await.unwrap(),
        vec![agent.instance.clone()]
    );
    let agent_id = AgentId {
        host: host.host_id(),
        instance: agent.instance.clone(),
    };

    let identity = ClientIdentity::generate().unwrap();
    let err = connect_relay(
        &endpoint,
        agent_id.clone(),
        &identity,
        &host.static_public(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not authorized"), "{err}");

    let (_, secret) = Pairings::in_dir(&agent.data)
        .create(Duration::from_secs(60))
        .unwrap();
    let uri = PairingUri {
        relay_url: relay.url.clone(),
        host: host.host_id(),
        instance: agent.instance.clone(),
        host_static: host.static_public(),
        secret: Zeroizing::new(*secret),
    };
    let parsed: PairingUri = uri.to_string().parse().unwrap();
    assert_eq!(parsed.relay_url, relay.url);
    pair_relay(
        &RelayEndpoint::new(&parsed.relay_url),
        &parsed,
        &identity,
        "laptop",
    )
    .await
    .unwrap();
    let again = pair_relay(&endpoint, &parsed, &identity, "laptop")
        .await
        .unwrap_err();
    assert!(again.to_string().contains("unknown or expired"), "{again}");

    let mut stream = connect_relay(
        &endpoint,
        agent_id.clone(),
        &identity,
        &host.static_public(),
    )
    .await
    .unwrap();
    stream.send(Bytes::from_static(b"hello")).await.unwrap();
    assert_eq!(
        stream.recv().await.unwrap().unwrap(),
        Bytes::from_static(b"laptop:hello")
    );
    let big = vec![7u8; 3 * 1024 * 1024];
    stream.send(Bytes::from(big.clone())).await.unwrap();
    let echoed = stream.recv().await.unwrap().unwrap();
    assert_eq!(echoed.len(), big.len() + 7);
    assert_eq!(relay.relay.open_channels(), 1);
    let metrics = relay.relay.metrics_text();
    assert!(metrics.contains("loom_relay_agents_online 1"), "{metrics}");
    assert!(metrics.contains("loom_relay_channels_open 1"), "{metrics}");
    assert!(
        !metrics.contains("loom_relay_frames_forwarded_total 0"),
        "{metrics}"
    );

    let wrong = connect_relay(&endpoint, agent_id.clone(), &identity, &[3; 32]).await;
    assert!(wrong.is_err());

    let missing = connect_relay(
        &endpoint,
        AgentId {
            host: host.host_id(),
            instance: InstanceId("nope".into()),
        },
        &identity,
        &host.static_public(),
    )
    .await
    .unwrap_err();
    assert!(matches!(missing, RelayError::Refused { .. }), "{missing}");

    let mut watcher = RelayClient::connect(&endpoint).await.unwrap();
    watcher.list_agents(host.host_id()).await.unwrap();
    agent.shutdown.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(10), stream.recv())
        .await
        .unwrap();
    assert!(!matches!(ended, Ok(Some(_))));
    for _ in 0..100 {
        if relay.relay.online_agents() == 0 && relay.relay.open_channels() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(relay.relay.online_agents(), 0);
    assert_eq!(relay.relay.open_channels(), 0);
    assert!(
        watcher
            .list_agents(host.host_id())
            .await
            .unwrap()
            .is_empty()
    );
    relay.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_unknown_hosts_and_limits_agents() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let relay = start_relay(RelayServerConfig::default()).await;
    let mut agent = start_agent(&relay.url, dir, host.clone(), "a");
    let state = wait_state(&mut agent.state, |s| {
        matches!(s, LinkState::Disconnected(_))
    })
    .await;
    assert!(
        matches!(&state, LinkState::Disconnected(m) if m.contains("allowed_hosts")),
        "{state:?}"
    );
    agent.shutdown.cancel();

    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut config = RelayServerConfig {
        allowed_hosts: vec![host.host_id().to_string()],
        ..Default::default()
    };
    config.limits.max_agents_per_host = 1;
    let relay = start_relay(config).await;
    let mut first = start_agent(&relay.url, dir, host.clone(), "one");
    wait_state(&mut first.state, |s| *s == LinkState::Registered).await;
    let second_dir = tempfile::tempdir().unwrap();
    let mut second = start_agent(&relay.url, second_dir, host.clone(), "two");
    let state = wait_state(&mut second.state, |s| {
        matches!(s, LinkState::Disconnected(_))
    })
    .await;
    assert!(
        matches!(&state, LinkState::Disconnected(m) if m.contains("too many agents")),
        "{state:?}"
    );
    first.shutdown.cancel();
    second.shutdown.cancel();
    relay.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn access_tokens_are_required_when_configured() {
    let server = wiremock::MockServer::start().await;
    let config = RelayServerConfig {
        cloudflare_access: Some(loom_config::AccessVerifyConfig {
            team_domain: "team.cloudflareaccess.com".into(),
            audience: "aud".into(),
            certs_url: Some(format!("{}/certs", server.uri())),
        }),
        ..Default::default()
    };
    let relay = start_relay(config).await;
    let err = RelayClient::connect(&RelayEndpoint::new(&relay.url))
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("403"), "{err}");
    relay.shutdown.cancel();
}

#[tokio::test]
async fn verifies_access_jwts() {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/certs"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string(include_str!("fixtures/test_jwks.json")),
        )
        .mount(&server)
        .await;
    let verifier = loom_relay::AccessVerifier::new(loom_config::AccessVerifyConfig {
        team_domain: "team.cloudflareaccess.com".into(),
        audience: "aud-tag".into(),
        certs_url: Some(format!("{}/certs", server.uri())),
    });
    let key = EncodingKey::from_rsa_pem(include_bytes!("fixtures/test_rsa.pem")).unwrap();
    let token = |aud: &str, iss: &str, exp: u64, kid: &str| {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        encode(
            &header,
            &serde_json::json!({"aud": [aud], "iss": iss, "exp": exp, "sub": "svc"}),
            &key,
        )
        .unwrap()
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let issuer = "https://team.cloudflareaccess.com";
    verifier
        .verify(&token("aud-tag", issuer, now + 60, "test-key"))
        .await
        .unwrap();
    for bad in [
        token("other", issuer, now + 60, "test-key"),
        token("aud-tag", "https://evil", now + 60, "test-key"),
        token("aud-tag", issuer, now - 3600, "test-key"),
        token("aud-tag", issuer, now + 60, "unknown"),
        "garbage".to_owned(),
    ] {
        assert!(verifier.verify(&bad).await.is_err(), "{bad}");
    }
}

#[derive(Default)]
struct TestHosts(std::sync::Mutex<HashSet<HostId>>);

impl HostCheck for TestHosts {
    fn check(&self, host: &HostId) -> Result<(), String> {
        if self.0.lock().unwrap().contains(host) {
            Ok(())
        } else {
            Err(format!("host {host} is not registered"))
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_check_decides_which_agents_register() {
    let hosts = Arc::new(TestHosts::default());
    let relay = start_relay_with_check(RelayServerConfig::default(), hosts.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut rejected = start_agent(&relay.url, dir, host.clone(), "a");
    let state = wait_state(&mut rejected.state, |s| {
        matches!(s, LinkState::Disconnected(_))
    })
    .await;
    assert!(
        matches!(&state, LinkState::Disconnected(m) if m.contains("is not registered")),
        "{state:?}"
    );
    rejected.shutdown.cancel();

    hosts.0.lock().unwrap().insert(host.host_id());
    let dir = tempfile::tempdir().unwrap();
    let mut agent = start_agent(&relay.url, dir, host.clone(), "b");
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    assert_eq!(relay.relay.online_agents(), 1);

    hosts.0.lock().unwrap().clear();
    assert_eq!(relay.relay.disconnect_host(&host.host_id()), 1);
    let state = wait_state(
        &mut agent.state,
        |s| matches!(s, LinkState::Disconnected(m) if m.contains("is not registered")),
    )
    .await;
    assert!(matches!(state, LinkState::Disconnected(_)));
    assert_eq!(relay.relay.online_agents(), 0);
    agent.shutdown.cancel();
    relay.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn registered_agents_leave_the_limits_of_their_address() {
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut config = RelayServerConfig {
        allowed_hosts: vec![host.host_id().to_string()],
        ..Default::default()
    };
    config.limits.max_connections_per_ip = 2;
    let relay = start_relay(config).await;
    let mut agents = Vec::new();
    for name in ["a", "b", "c"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = start_agent(&relay.url, dir, host.clone(), name);
        wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
        agents.push(agent);
    }
    assert_eq!(relay.relay.online_agents(), 3);

    let endpoint = RelayEndpoint::new(&relay.url);
    let _one = RelayClient::connect(&endpoint).await.unwrap();
    let _two = RelayClient::connect(&endpoint).await.unwrap();
    let err = RelayClient::connect(&endpoint).await.err().unwrap();
    assert!(err.to_string().contains("429"), "{err}");
    for agent in agents {
        agent.shutdown.cancel();
    }
    relay.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn local_agents_take_channels_and_vouch_for_their_clients() {
    let mut config = RelayServerConfig::default();
    config.limits.max_connections_per_ip = 1;
    let relay = start_relay(config).await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let agent_id = AgentId {
        host: host.host_id(),
        instance: InstanceId("oracle".into()),
    };
    let local = relay.relay.attach_local(agent_id.clone()).unwrap();
    assert_eq!(relay.relay.online_agents(), 1);
    let responder = Arc::new(FileResponder::in_dir(dir.path()));
    let identity = ClientIdentity::generate().unwrap();
    responder
        .clients
        .authorize(identity.public_key(), "laptop")
        .unwrap();
    let (tx, mut incoming) = mpsc::channel(8);
    let shutdown = CancellationToken::new();
    tokio::spawn(serve_local_link(
        LocalLink {
            tx: local.tx,
            rx: local.rx,
        },
        host.clone(),
        responder,
        tx,
        shutdown.clone(),
    ));

    let endpoint = RelayEndpoint::new(&relay.url);
    let mut stream = connect_relay(
        &endpoint,
        agent_id.clone(),
        &identity,
        &host.static_public(),
    )
    .await
    .unwrap();
    let mut channel = tokio::time::timeout(Duration::from_secs(10), incoming.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(channel.peer.label, "laptop");
    stream.send(Bytes::from_static(b"ping")).await.unwrap();
    assert_eq!(
        channel.stream.recv().await.unwrap().unwrap(),
        Bytes::from_static(b"ping")
    );
    channel
        .stream
        .send(Bytes::from_static(b"pong"))
        .await
        .unwrap();
    assert_eq!(
        stream.recv().await.unwrap().unwrap(),
        Bytes::from_static(b"pong")
    );

    let err = RelayClient::connect(&endpoint).await.err().unwrap();
    assert!(err.to_string().contains("429"), "{err}");
    assert!(
        !relay
            .relay
            .vouch(channel.channel + 1000, &channel.peer.pubkey)
    );
    assert!(relay.relay.vouch(channel.channel, &channel.peer.pubkey));
    assert!(!relay.relay.vouch(channel.channel, &[9; 32]));
    let _second = RelayClient::connect(&endpoint).await.unwrap();

    shutdown.cancel();
    relay.shutdown.cancel();
}

/// A relay of version 0.2, which speaks only version 1 of the protocol:
/// it refuses other versions, and registers agents without checking them.
/// Upgrading it ends its connections, as a restart would, and then it
/// speaks version 2 as well.
struct OldRelay {
    url: String,
    /// The versions agents said, in order.
    versions: mpsc::UnboundedReceiver<u16>,
    upgrade: watch::Sender<bool>,
}

impl OldRelay {
    async fn next_version(&mut self) -> Option<u16> {
        tokio::time::timeout(Duration::from_secs(10), self.versions.recv())
            .await
            .expect("timed out waiting for an agent")
    }
}

async fn start_old_relay() -> OldRelay {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use futures::StreamExt;
    use loom_proto::relay::{HostId, RelayErrorCode, RelayFrame};
    use loom_proto::{decode, encode};

    async fn read(socket: &mut WebSocket) -> Option<RelayFrame> {
        loop {
            match socket.next().await?.ok()? {
                Message::Binary(bytes) => return decode(&bytes).ok(),
                Message::Close(_) => return None,
                _ => {}
            }
        }
    }
    async fn write(socket: &mut WebSocket, frame: RelayFrame) {
        let bytes = encode(&frame).unwrap();
        socket.send(Message::Binary(bytes.into())).await.unwrap();
    }

    let (tx, versions) = mpsc::unbounded_channel();
    let (upgrade, upgraded) = watch::channel(false);
    let app = axum::Router::new().route(
        "/v1/agent",
        axum::routing::get(move |ws: WebSocketUpgrade| {
            let tx = tx.clone();
            let mut upgraded = upgraded.clone();
            async move {
                ws.on_upgrade(move |mut socket| async move {
                    let Some(RelayFrame::AgentHello { version, .. }) = read(&mut socket).await
                    else {
                        return;
                    };
                    let _ = tx.send(version);
                    let new = *upgraded.borrow_and_update();
                    if version != 1 && !(new && version == 2) {
                        let message = format!("the relay speaks version 1, not {version}");
                        let code = RelayErrorCode::VersionMismatch;
                        write(&mut socket, RelayFrame::Error { code, message }).await;
                        let _ = socket.send(Message::Close(None)).await;
                        return;
                    }
                    write(&mut socket, RelayFrame::Challenge { nonce: [0; 32] }).await;
                    let Some(RelayFrame::AgentAuth { .. }) = read(&mut socket).await else {
                        return;
                    };
                    let host = HostId([0; 32]);
                    write(&mut socket, RelayFrame::Registered { host }).await;
                    loop {
                        tokio::select! {
                            frame = read(&mut socket) => match frame {
                                // The old relay knows no notifications.
                                Some(frame) => {
                                    assert!(new || !matches!(frame, RelayFrame::Push { .. }));
                                }
                                None => break,
                            },
                            // Restarted as the new version.
                            Ok(()) = upgraded.changed(), if !new => {
                                let _ = socket.send(Message::Close(None)).await;
                                break;
                            }
                        }
                    }
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    OldRelay {
        url: format!("ws://{addr}"),
        versions,
        upgrade,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_fall_back_to_an_older_relay_without_notifications() {
    let mut relay = start_old_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut agent = start_agent(&relay.url, dir, host, "inst-old");
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    assert_eq!(relay.next_version().await, Some(2));
    assert_eq!(relay.next_version().await, Some(1));
    agent.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_ask_for_notifications_again_when_their_relay_comes_back() {
    let mut relay = start_old_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let host = host_in(dir.path());
    let mut agent = start_agent(&relay.url, dir, host, "inst-upgraded");
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    assert_eq!(relay.next_version().await, Some(2));
    assert_eq!(relay.next_version().await, Some(1));

    // The relay restarts as a version with notifications, and the agent
    // that fell back asks for them again instead of staying on version 1.
    relay.upgrade.send(true).unwrap();
    assert_eq!(relay.next_version().await, Some(2));
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    assert!(relay.versions.try_recv().is_err());
    agent.shutdown.cancel();
}
