use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use loom_config::RelayServerConfig;
use loom_crypto::{ClientIdentity, FileResponder, HostIdentity, PairingUri, Pairings, Zeroizing};
use loom_proto::InstanceId;
use loom_proto::relay::AgentId;
use loom_relay::{Relay, serve};
use loom_transport::{
    AgentLinkOptions, IncomingChannel, LinkState, RelayClient, RelayEndpoint, RelayError,
    connect_relay, pair_relay, spawn_agent_link,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

struct TestRelay {
    url: String,
    relay: Arc<Relay>,
    shutdown: CancellationToken,
}

async fn start_relay(config: RelayServerConfig) -> TestRelay {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let relay = Relay::new(config);
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
