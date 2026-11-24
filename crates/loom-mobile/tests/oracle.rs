use std::sync::Arc;
use std::time::Duration;

use loom_client::oracle_proto::{Repository, Role};
use loom_client::{ClientIdentity, pair_oracle};
use loom_config::{Dirs, OracleConfig};
use loom_mobile::{Loom, OracleConnection, OracleLists};
use loom_oracle::Oracle;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

fn dirs(root: &std::path::Path) -> Dirs {
    Dirs {
        home: root.to_path_buf(),
        config_home: root.join("config"),
        data_home: root.join("data"),
        state_home: root.join("state"),
        runtime_dir: None,
    }
}

async fn start_oracle(root: &std::path::Path) -> (Arc<Oracle>, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = OracleConfig {
        url: Some(format!("ws://{addr}")),
        listen: addr.to_string(),
        ..Default::default()
    };
    let oracle = Oracle::open(&config, &dirs(root)).unwrap();
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
    (oracle, shutdown)
}

async fn wait_lists(
    connection: &Arc<OracleConnection>,
    check: impl Fn(&OracleLists) -> bool,
) -> OracleLists {
    let lists = connection.lists();
    if check(&lists) {
        return lists;
    }
    tokio::time::timeout(WAIT, async {
        loop {
            let lists = connection.clone().next_lists().await.unwrap();
            if check(&lists) {
                return lists;
            }
        }
    })
    .await
    .expect("timed out waiting for the lists")
}

fn repository(url: &str, directory: Option<&str>) -> Repository {
    Repository {
        url: url.into(),
        provider: None,
        directory: directory.map(str::to_owned),
        base_branch: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_pairs_with_an_oracle_and_follows_its_lists() {
    let dir = tempfile::tempdir().unwrap();
    let (oracle, shutdown) = start_oracle(&dir.path().join("oracle")).await;
    let host_link = oracle
        .create_pairing(Role::Host, Duration::from_secs(60), None)
        .unwrap()
        .link;
    pair_oracle(
        &host_link,
        &ClientIdentity::generate().unwrap(),
        "box",
        None,
    )
    .await
    .unwrap();
    let host = oracle.store().peers(Some(Role::Host)).unwrap()[0].id;
    oracle
        .store()
        .create_workspace(
            "acme",
            host,
            &[
                repository("https://github.com/acme/server.git", None),
                repository("https://github.com/acme/web.git", Some("client")),
            ],
            &[],
        )
        .unwrap();
    oracle.set_preset("dev", "", None, None, None).unwrap();

    let app_dir = dir.path().join("app");
    let loom = Loom::new(app_dir.to_string_lossy().into_owned()).unwrap();
    let link = oracle
        .create_pairing(Role::Device, Duration::from_secs(60), None)
        .unwrap()
        .link;
    let paired = loom
        .clone()
        .pair(link, "phone".into(), "next".into())
        .await
        .unwrap();
    assert_eq!(paired.name, "next");
    assert_eq!(loom.hosts(), std::slice::from_ref(&paired));

    let connection = loom.clone().connect_oracle("next".into()).await.unwrap();
    let lists = wait_lists(&connection, |l| l.connected && !l.workspaces.is_empty()).await;
    assert_eq!(lists.workspaces[0].name, "acme");
    let directories: Vec<&str> = lists.workspaces[0]
        .repositories
        .iter()
        .map(|r| r.directory.as_str())
        .collect();
    assert_eq!(directories, ["server", "client"]);
    assert_eq!(lists.presets[0].name, "dev");
    assert!(lists.codespaces.is_empty());

    let form = connection.new_codespace_form("acme".into()).unwrap();
    assert_eq!(form.preset.as_deref(), Some("dev"));
    assert_eq!((form.cpus, form.memory.as_str()), (2, "4 GiB"));
    assert_eq!(form.host, "box");
    assert_eq!(form.host_detail.as_deref(), Some("not connected"));
    assert!(connection.new_codespace_form("nope".into()).is_err());

    let warnings = connection
        .clone()
        .codespace_warnings("acme".into(), Some("dev".into()))
        .await
        .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(
        connection
            .clone()
            .codespace_warnings("missing".into(), None)
            .await
            .is_err()
    );

    oracle.set_preset("ops", "", None, None, None).unwrap();
    wait_lists(&connection, |l| l.presets.iter().any(|p| p.name == "ops")).await;
    connection
        .clone()
        .set_push_token(Some("phone-token".into()))
        .await
        .unwrap();
    assert!(
        oracle
            .device_keys()
            .unwrap()
            .iter()
            .any(|d| d.label == "phone" && d.push_token.as_deref() == Some("phone-token"))
    );
    assert!(
        connection
            .clone()
            .connect_codespace("nope".into())
            .await
            .is_err()
    );
    assert!(matches!(
        loom.clone().connect("next".into(), "missing".into()).await,
        Err(loom_mobile::LoomError::Offline { .. })
    ));

    connection.disconnect();
    assert!(connection.clone().next_lists().await.is_none());
    shutdown.cancel();
}
