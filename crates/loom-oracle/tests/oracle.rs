use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use loom_client::oracle_proto::{ErrorCode, Event, Role};
use loom_client::{
    ClientError, ClientIdentity, OracleClient, OracleEndpoint, OracleEvents, pair_oracle,
};
use loom_config::{Dirs, OracleConfig};
use loom_crypto::{FileResponder, HostIdentity, PairingUri, Pairings, Zeroizing};
use loom_oracle::Oracle;
use loom_proto::InstanceId;
use loom_proto::relay::AgentId;
use loom_transport::{
    AgentLinkOptions, IncomingChannel, LinkState, RelayEndpoint, connect_relay, pair_relay,
    spawn_agent_link,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

struct TestOracle {
    dir: tempfile::TempDir,
    config: OracleConfig,
    oracle: Arc<Oracle>,
    url: String,
    shutdown: CancellationToken,
}

fn dirs_in(root: &std::path::Path) -> Dirs {
    let home = root.join("home");
    Dirs {
        config_home: home.join(".config"),
        data_home: home.join(".local/share"),
        state_home: home.join(".local/state"),
        runtime_dir: None,
        home,
    }
}

async fn start_oracle_in(dir: tempfile::TempDir) -> TestOracle {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = OracleConfig {
        url: Some(format!("ws://{addr}")),
        listen: addr.to_string(),
        ..Default::default()
    };
    let oracle = Oracle::open(&config, &dirs_in(dir.path())).unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(loom_oracle::serve(
        oracle.clone(),
        listener,
        shutdown.clone(),
    ));
    for _ in 0..200 {
        if oracle.relay().online_agents() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(oracle.relay().online_agents(), 1);
    TestOracle {
        dir,
        config,
        url: format!("ws://{addr}"),
        oracle,
        shutdown,
    }
}

async fn start_oracle() -> TestOracle {
    start_oracle_in(tempfile::tempdir().unwrap()).await
}

impl TestOracle {
    fn dirs(&self) -> Dirs {
        dirs_in(self.dir.path())
    }

    fn link(&self, role: Role) -> String {
        self.oracle
            .create_pairing(role, Duration::from_secs(60), None)
            .unwrap()
            .link
    }

    async fn pair(&self, role: Role, label: &str) -> (ClientIdentity, OracleEndpoint) {
        let identity = ClientIdentity::generate().unwrap();
        let endpoint = pair_oracle(&self.link(role), &identity, label, None)
            .await
            .unwrap();
        (identity, endpoint)
    }

    async fn device(&self, label: &str) -> (OracleClient, OracleEvents) {
        let (identity, endpoint) = self.pair(Role::Device, label).await;
        OracleClient::connect(&endpoint, &identity, "test")
            .await
            .unwrap()
    }
}

async fn next_event(events: &mut OracleEvents, check: impl Fn(&Event) -> bool) -> Event {
    loop {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("the events ended");
        if check(&event) {
            return event;
        }
    }
}

fn code(err: ClientError) -> ErrorCode {
    match err {
        ClientError::Oracle(err) => err.code,
        other => panic!("expected an oracle error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_pair_and_use_the_control_service() {
    let oracle = start_oracle().await;
    let link = oracle.link(Role::Device);
    assert!(link.starts_with("loom+ws://127.0.0.1:"), "{link}");
    let laptop_key = ClientIdentity::generate().unwrap();
    let endpoint = pair_oracle(&link, &laptop_key, "laptop", None)
        .await
        .unwrap();
    assert_eq!(endpoint.host, oracle.oracle.identity().host_id());
    assert_eq!(endpoint.url, oracle.url);
    let again = pair_oracle(&link, &ClientIdentity::generate().unwrap(), "other", None)
        .await
        .unwrap_err();
    assert!(again.to_string().contains("unknown or expired"), "{again}");

    let (laptop, mut events) = OracleClient::connect(&endpoint, &laptop_key, "test")
        .await
        .unwrap();
    let hello = laptop.hello().clone();
    assert_eq!(hello.peer.role, Role::Device);
    assert_eq!(hello.peer.label, "laptop");
    assert_eq!(
        hello.oracle.host_id,
        oracle.oracle.identity().host_id().to_string()
    );
    assert_eq!(laptop.info().await.unwrap(), hello.oracle);
    let devices = laptop.devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, hello.peer.id);
    assert!(devices[0].connected);
    assert!(devices[0].last_seen.is_some());

    laptop.subscribe().await.unwrap();
    let pairing = laptop
        .create_device_pairing(Some(Duration::from_secs(120)))
        .await
        .unwrap();
    assert_eq!(pairing.pairing.role, Role::Device);
    assert_eq!(pairing.pairing.created_by, Some(hello.peer.id));
    assert_eq!(pairing.pairing.expires_at - pairing.pairing.created_at, 120);
    let phone_key = ClientIdentity::generate().unwrap();
    let phone_endpoint = pair_oracle(&pairing.link, &phone_key, "phone", None)
        .await
        .unwrap();
    let id = pairing.pairing.id.clone();
    next_event(
        &mut events,
        |e| matches!(e, Event::PairingRemoved { id: i, used: true } if *i == id),
    )
    .await;
    let Event::DeviceChanged { device: phone } = next_event(
        &mut events,
        |e| matches!(e, Event::DeviceChanged { device } if device.label == "phone"),
    )
    .await
    else {
        unreachable!()
    };

    let (phone_client, _phone_events) = OracleClient::connect(&phone_endpoint, &phone_key, "test")
        .await
        .unwrap();
    next_event(&mut events, |e| {
        matches!(e, Event::DeviceChanged { device } if device.id == phone.id && device.connected)
    })
    .await;
    phone_client.set_push_token(Some("token-1")).await.unwrap();
    let renamed = laptop.rename_device(phone.id, "pixel").await.unwrap();
    assert_eq!(renamed.label, "pixel");
    assert!(renamed.push);
    assert_eq!(
        code(laptop.rename_device(phone.id, "laptop").await.unwrap_err()),
        ErrorCode::Conflict
    );
    assert_eq!(
        code(
            laptop
                .rename_device(phone.id, "bad label")
                .await
                .unwrap_err()
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(phone_client.set_push_token(Some("")).await.unwrap_err()),
        ErrorCode::BadRequest
    );

    laptop.revoke_device(phone.id).await.unwrap();
    next_event(
        &mut events,
        |e| matches!(e, Event::DeviceRemoved { id } if *id == phone.id),
    )
    .await;
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match phone_client.devices().await {
                Err(ClientError::Closed) => return,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the revoked device kept its connection");
    let refused = OracleClient::connect(&phone_endpoint, &phone_key, "test")
        .await
        .err()
        .unwrap();
    assert!(refused.to_string().contains("not paired"), "{refused}");
    assert_eq!(
        code(laptop.revoke_device(phone.id).await.unwrap_err()),
        ErrorCode::NotFound
    );

    let pending = laptop.create_device_pairing(None).await.unwrap();
    assert_eq!(pending.pairing.expires_at - pending.pairing.created_at, 600);
    assert_eq!(
        laptop.pairings().await.unwrap(),
        vec![pending.pairing.clone()]
    );
    laptop.cancel_pairing(&pending.pairing.id).await.unwrap();
    assert!(laptop.pairings().await.unwrap().is_empty());
    assert_eq!(
        code(
            laptop
                .cancel_pairing(&pending.pairing.id)
                .await
                .unwrap_err()
        ),
        ErrorCode::NotFound
    );
    let refused = pair_oracle(
        &pending.link,
        &ClientIdentity::generate().unwrap(),
        "x",
        None,
    )
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains("unknown or expired"),
        "{refused}"
    );
    assert_eq!(
        code(
            laptop
                .create_device_pairing(Some(Duration::from_secs(30 * 24 * 3600)))
                .await
                .unwrap_err()
        ),
        ErrorCode::BadRequest
    );

    let actions: Vec<String> = oracle
        .oracle
        .store()
        .audit_log(100)
        .unwrap()
        .into_iter()
        .map(|row| row.action)
        .collect();
    for action in [
        "peer_paired",
        "pairing_created",
        "device_renamed",
        "device_revoked",
        "pairing_cancelled",
    ] {
        assert!(
            actions.iter().any(|a| a == action),
            "{action} in {actions:?}"
        );
    }
    oracle.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn hosts_and_codespaces_cannot_use_device_operations() {
    let oracle = start_oracle().await;
    for (role, label) in [(Role::Host, "office-1"), (Role::Codespace, "cs-1")] {
        let (identity, endpoint) = oracle.pair(role, label).await;
        let (client, _events) = OracleClient::connect(&endpoint, &identity, "test")
            .await
            .unwrap();
        assert_eq!(client.hello().peer.role, role);
        assert_eq!(client.hello().peer.label, label);
        client.info().await.unwrap();
        for err in [
            client.devices().await.unwrap_err(),
            client.subscribe().await.unwrap_err(),
            client.create_device_pairing(None).await.unwrap_err(),
        ] {
            assert_eq!(code(err), ErrorCode::Forbidden);
        }
    }
    let (laptop, _events) = oracle.device("laptop").await;
    assert_eq!(laptop.devices().await.unwrap().len(), 1);
    oracle.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_malformed_requests_get_errors() {
    let oracle = start_oracle().await;
    let (identity, endpoint) = oracle.pair(Role::Device, "laptop").await;
    let mut stream = endpoint.connect_stream(&identity).await.unwrap();
    stream
        .send(Bytes::from_static(
            br#"{"type":"request","id":1,"command":{"type":"oracle_info"}}"#,
        ))
        .await
        .unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(&stream.recv().await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["type"], "rejected");

    let mut stream = endpoint.connect_stream(&identity).await.unwrap();
    for message in [
        r#"{"type":"hello","version":99,"client":"future","features":["x"]}"#,
        r#"{"type":"request","id":2,"command":{"type":"make_coffee"}}"#,
        r#"{"type":"request","id":3,"command":{"type":"revoke_device"}}"#,
        r#"{"type":"request","id":4,"command":{"type":"list_devices","since":1}}"#,
    ] {
        stream.send(Bytes::from(message)).await.unwrap();
    }
    let mut replies = Vec::new();
    for _ in 0..4 {
        let frame = stream.recv().await.unwrap().unwrap();
        replies.push(serde_json::from_slice::<serde_json::Value>(&frame).unwrap());
    }
    assert_eq!(replies[0]["type"], "hello");
    assert_eq!(replies[0]["peer"]["role"], "device");
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["error"]["code"], "unknown_command");
    assert_eq!(replies[2]["id"], 3);
    assert_eq!(replies[2]["error"]["code"], "bad_request");
    assert_eq!(replies[3]["id"], 4);
    assert_eq!(replies[3]["result"]["devices"][0]["label"], "laptop");
    oracle.shutdown.cancel();
}

struct TestAgent {
    _dir: tempfile::TempDir,
    data: PathBuf,
    host: Arc<HostIdentity>,
    instance: InstanceId,
    state: watch::Receiver<LinkState>,
    shutdown: CancellationToken,
}

fn start_agent(url: &str, name: &str) -> TestAgent {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let host = Arc::new(HostIdentity::load_or_create(&data).unwrap());
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
        host,
        instance,
        state,
        shutdown,
    }
}

async fn wait_state(
    state: &mut watch::Receiver<LinkState>,
    check: impl Fn(&LinkState) -> bool,
) -> LinkState {
    tokio::time::timeout(WAIT, state.wait_for(|s| check(s)))
        .await
        .expect("timed out waiting for the link")
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn current_agents_and_clients_use_the_oracle_as_their_relay() {
    let oracle = start_oracle().await;
    let (laptop, mut events) = oracle.device("laptop").await;
    laptop.subscribe().await.unwrap();

    let mut agent = start_agent(&oracle.url, "inst-1");
    let state = wait_state(&mut agent.state, |s| {
        matches!(s, LinkState::Disconnected(_))
    })
    .await;
    assert!(
        matches!(&state, LinkState::Disconnected(m) if m.contains("not a relay host")),
        "{state:?}"
    );
    agent.shutdown.cancel();

    let mut agent = start_agent(&oracle.url, "inst-1");
    let host_id = agent.host.host_id().to_string();
    let added = laptop.add_relay_host(&host_id, "devbox").await.unwrap();
    assert_eq!(added.label, "devbox");
    next_event(
        &mut events,
        |e| matches!(e, Event::RelayHostChanged { relay_host } if relay_host.host_id == host_id),
    )
    .await;
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    let hosts = laptop.relay_hosts().await.unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].agents, vec!["inst-1".to_owned()]);

    let endpoint = RelayEndpoint::new(&oracle.url);
    let (_, secret) = Pairings::in_dir(&agent.data)
        .create(Duration::from_secs(60))
        .unwrap();
    let uri = PairingUri {
        relay_url: oracle.url.clone(),
        host: agent.host.host_id(),
        instance: agent.instance.clone(),
        host_static: agent.host.static_public(),
        secret: Zeroizing::new(*secret),
    };
    let classic = ClientIdentity::generate().unwrap();
    pair_relay(&endpoint, &uri, &classic, "desktop")
        .await
        .unwrap();
    let agent_id = AgentId {
        host: agent.host.host_id(),
        instance: agent.instance.clone(),
    };
    let mut stream = connect_relay(&endpoint, agent_id, &classic, &agent.host.static_public())
        .await
        .unwrap();
    stream.send(Bytes::from_static(b"hello")).await.unwrap();
    assert_eq!(
        stream.recv().await.unwrap().unwrap(),
        Bytes::from_static(b"desktop:hello")
    );

    assert_eq!(
        code(
            laptop
                .add_relay_host(&oracle.oracle.identity().host_id().to_string(), "self")
                .await
                .unwrap_err()
        ),
        ErrorCode::Conflict
    );
    assert_eq!(
        code(laptop.add_relay_host("abc", "x").await.unwrap_err()),
        ErrorCode::BadRequest
    );

    laptop.remove_relay_host(&host_id).await.unwrap();
    next_event(
        &mut events,
        |e| matches!(e, Event::RelayHostRemoved { host_id: h } if *h == host_id),
    )
    .await;
    let state = wait_state(
        &mut agent.state,
        |s| matches!(s, LinkState::Disconnected(m) if m.contains("not a relay host")),
    )
    .await;
    assert!(matches!(state, LinkState::Disconnected(_)));
    let ended = tokio::time::timeout(WAIT, stream.recv()).await.unwrap();
    assert!(!matches!(ended, Ok(Some(_))));
    assert!(laptop.relay_hosts().await.unwrap().is_empty());
    assert_eq!(
        code(laptop.remove_relay_host(&host_id).await.unwrap_err()),
        ErrorCode::NotFound
    );
    agent.shutdown.cancel();
    oracle.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_store_keeps_peers_and_needs_its_master_key() {
    let oracle = start_oracle().await;
    let (identity, endpoint) = oracle.pair(Role::Device, "laptop").await;
    let mut agent = start_agent(&oracle.url, "inst-1");
    oracle
        .oracle
        .add_relay_host(&agent.host.host_id().to_string(), "devbox", None)
        .unwrap();
    wait_state(&mut agent.state, |s| *s == LinkState::Registered).await;
    agent.shutdown.cancel();
    oracle.shutdown.cancel();
    let TestOracle {
        dir,
        config,
        oracle: old,
        ..
    } = oracle;
    let host = old.identity().host_id();
    drop(old);

    let dirs = dirs_in(dir.path());
    let key = config.master_key_path(&dirs);
    let saved = std::fs::read(&key).unwrap();
    std::fs::write(&key, format!("{}\n", loom_crypto::encode_key(&[7; 32]))).unwrap();
    let err = Oracle::open(&config, &dirs).err().unwrap();
    assert!(format!("{err:#}").contains("does not match"), "{err:#}");
    std::fs::remove_file(&key).unwrap();
    let err = Oracle::open(&config, &dirs).err().unwrap();
    assert!(format!("{err:#}").contains("is missing"), "{err:#}");
    std::fs::write(&key, saved).unwrap();

    let restarted = start_oracle_in(dir).await;
    assert_eq!(restarted.oracle.identity().host_id(), host);
    let (client, _events) = OracleClient::connect(
        &OracleEndpoint {
            url: restarted.url.clone(),
            ..endpoint
        },
        &identity,
        "test",
    )
    .await
    .unwrap();
    assert_eq!(client.hello().peer.label, "laptop");
    assert_eq!(client.relay_hosts().await.unwrap()[0].label, "devbox");
    assert_eq!(
        restarted.config.data_dir(&restarted.dirs()),
        dirs.data_home.join("loom/oracle")
    );
    restarted.shutdown.cancel();
}
