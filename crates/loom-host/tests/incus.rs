use std::path::PathBuf;

use loom_client::oracle_proto::{Bootstrap, CreateInstance, Environment};
use loom_host::{
    AGENT_UNIT, Agent, CheckStatus, Driver, IMAGE_PREFIX, IncusDriver, KEY_CODESPACE, KEY_ORACLE,
    Progress, environment_hash,
};
use loom_incus::fake::FakeIncus;
use tokio::sync::mpsc;

const ORACLE: &str = "oracle-1";

fn request(id: &str, environment: Environment) -> CreateInstance {
    CreateInstance {
        codespace: id.into(),
        instance: format!("loom-{id}"),
        cpus: 2,
        memory_mib: 4096,
        environment,
        bootstrap: Bootstrap {
            link: "loom+wss://oracle.example.com/x".into(),
            codespace: id.into(),
            workspaces_root: None,
        },
        agent_version: None,
    }
}

fn steps() -> (Progress, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Progress::new(tx), rx)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut all = Vec::new();
    while let Ok(step) = rx.try_recv() {
        all.push(step);
    }
    all
}

#[tokio::test]
async fn codespaces_are_instances_of_a_built_image() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    fake.state().reject_zstd = true;
    let driver = IncusDriver::new(
        fake.client(Some("loom")),
        "default".into(),
        "incusbr0".into(),
    );
    let binary = dir.path().join("loom-agent");
    std::fs::write(&binary, b"agent").unwrap();
    let agent = Agent {
        path: PathBuf::from(&binary),
        version: Some("0.5.0".into()),
    };

    let report = driver.report().await.unwrap();
    assert_eq!(report.driver, "incus");
    assert_eq!((report.cpus, report.memory_mib), (8, 16 * 1024));
    assert_eq!(report.storage_driver.as_deref(), Some("dir"));

    let mut environment = Environment::default();
    environment
        .variables
        .insert("HTTP_PROXY".into(), "http://proxy:3128".into());
    environment.setup_script = Some("echo setup".into());
    environment.docker.registry_mirrors = vec!["https://mirror.example.com".into()];
    let (progress, mut rx) = steps();
    driver
        .create(
            &request("c1", environment.clone()),
            ORACLE,
            &agent,
            &progress,
        )
        .await
        .unwrap();
    let all = drain(&mut rx);
    for step in [
        "image",
        "image: base script",
        "image: setup script",
        "image: publish",
        "instance",
        "agent",
        "start",
    ] {
        assert!(all.iter().any(|s| s == step), "missing {step} in {all:?}");
    }
    let alias = format!("{IMAGE_PREFIX}{}", environment_hash(&environment));
    {
        let state = fake.state();
        assert_eq!(state.images.len(), 1);
        assert_eq!(state.images[0].aliases[0].name, alias);
        assert!(
            state
                .instances
                .keys()
                .all(|name| !name.starts_with("loom-build-"))
        );
        let instance = &state.instances["loom-c1"];
        assert!(instance.instance.running());
        assert_eq!(instance.source["alias"], alias.as_str());
        let config = &instance.instance.config;
        assert_eq!(config["limits.cpu"], "2");
        assert_eq!(config["limits.memory"], "4096MiB");
        assert_eq!(config["security.nesting"], "true");
        assert_eq!(config[KEY_ORACLE], ORACLE);
        assert_eq!(config[KEY_CODESPACE], "c1");
        assert!(instance.instance.profiles.is_empty());
        assert_eq!(instance.instance.devices["root"]["pool"], "default");
        assert_eq!(instance.instance.devices["eth0"]["network"], "incusbr0");
        let files = &instance.files;
        assert_eq!(files["/usr/local/bin/loom-agent"].content, b"agent");
        assert_eq!(files["/usr/local/bin/loom-agent"].mode, "0755");
        let unit = &files["/etc/systemd/system/loom-agent.service"];
        assert_eq!(unit.content, AGENT_UNIT.as_bytes());
        let link = &files["/etc/systemd/system/multi-user.target.wants/loom-agent.service"];
        assert_eq!(link.kind, "symlink");
        assert_eq!(link.content, b"/etc/systemd/system/loom-agent.service");
        let bootstrap = &files["/home/dev/.config/loom/bootstrap.json"];
        assert_eq!((bootstrap.uid, bootstrap.mode.as_str()), (1000, "0600"));
        let bootstrap: Bootstrap = serde_json::from_slice(&bootstrap.content).unwrap();
        assert_eq!(
            bootstrap.workspaces_root,
            Some(PathBuf::from("/workspaces"))
        );
        let requests = state.requests.join("\n");
        assert!(
            requests.contains("POST /1.0/instances/loom-build-"),
            "{requests}"
        );
    }

    let listed = driver.list(ORACLE).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].codespace, "c1");
    assert!(listed[0].running);
    assert!(driver.list("other").await.unwrap().is_empty());

    let (progress, mut rx) = steps();
    driver
        .create(
            &request("c2", environment.clone()),
            ORACLE,
            &agent,
            &progress,
        )
        .await
        .unwrap();
    assert!(!drain(&mut rx).iter().any(|s| s.starts_with("image")));
    assert!(
        driver
            .create(
                &request("c2", environment.clone()),
                ORACLE,
                &agent,
                &Progress::none()
            )
            .await
            .is_err()
    );

    environment.setup_script = None;
    driver
        .create(
            &request("c3", environment.clone()),
            ORACLE,
            &agent,
            &Progress::none(),
        )
        .await
        .unwrap();
    {
        let state = fake.state();
        assert_eq!(state.images.len(), 1);
        assert_eq!(
            state.images[0].aliases[0].name,
            format!("{IMAGE_PREFIX}{}", environment_hash(&environment))
        );
    }

    assert!(driver.delete("other", "loom-c1").await.is_err());
    driver.delete(ORACLE, "loom-c1").await.unwrap();
    driver.delete(ORACLE, "loom-c1").await.unwrap();
    assert!(driver.delete(ORACLE, "c1").await.is_err());
    assert_eq!(driver.list(ORACLE).await.unwrap().len(), 2);

    let items = driver.check(false).await;
    let failed: Vec<_> = items
        .iter()
        .filter(|item| item.status == CheckStatus::Failed)
        .map(|item| item.name.as_str())
        .collect();
    assert_eq!(failed, vec!["project"]);
    assert!(driver.setup_project().await.unwrap());
    assert!(!driver.setup_project().await.unwrap());
    assert!(
        driver
            .check(false)
            .await
            .iter()
            .all(|item| item.status == CheckStatus::Ok)
    );
    let deep = driver.check(true).await;
    let docker = deep.iter().find(|item| item.name == "docker").unwrap();
    assert_eq!(docker.status, CheckStatus::Ok, "{docker:?}");
    assert!(
        fake.state()
            .instances
            .keys()
            .all(|name| !name.starts_with("loom-check-"))
    );
}

#[tokio::test]
async fn a_failed_setup_script_leaves_no_instance() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    fake.state().exec = Some(Box::new(|_, command| {
        if command.iter().any(|arg| arg == "/root/loom-setup.sh") {
            (2, String::new(), "E: package missing\n".into())
        } else {
            (0, String::new(), String::new())
        }
    }));
    let driver = IncusDriver::new(
        fake.client(Some("loom")),
        "default".into(),
        "incusbr0".into(),
    );
    let binary = dir.path().join("loom-agent");
    std::fs::write(&binary, b"agent").unwrap();
    let agent = Agent {
        path: binary,
        version: None,
    };
    let environment = Environment {
        setup_script: Some("apt-get install -y missing".into()),
        ..Environment::default()
    };
    let err = driver
        .create(
            &request("c1", environment),
            ORACLE,
            &agent,
            &Progress::none(),
        )
        .await
        .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("E: package missing"), "{message}");
    let state = fake.state();
    assert!(state.instances.is_empty());
    assert!(state.images.is_empty());
}
