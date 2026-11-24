use std::path::PathBuf;

use loom_client::oracle_proto::{
    Bootstrap, CreateInstance, Environment, InstanceVolume, VolumeKind,
};
use loom_config::{AclAction, HostRules, HostUser, NetworkRules};
use loom_host::{
    AGENT_UNIT, Agent, CheckStatus, Driver, IMAGE_PREFIX, IncusDriver, KEY_CODESPACE, KEY_ORACLE,
    Progress, SetupResult, environment_hash,
};
use loom_incus::fake::{FakeFile, FakeIncus, FakeInstance};
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
        volumes: Vec::new(),
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
        HostRules::default(),
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
        let nic = &instance.instance.devices["eth0"];
        assert_eq!(nic["security.port_isolation"], "true");
        assert_eq!(nic["security.ipv4_filtering"], "true");
        assert!(nic.get("security.acls").is_none());
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

    let (writer, mut reader) = tokio::io::duplex(1024);
    let code = loom_host::stdio(
        &driver,
        ORACLE,
        "c1",
        Box::new(&b"{\"hello\":1}\n"[..]),
        Box::new(writer),
        Box::new(tokio::io::sink()),
    )
    .await
    .unwrap();
    let mut echoed = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut echoed)
        .await
        .unwrap();
    assert_eq!((code, echoed.as_slice()), (0, &b"{\"hello\":1}\n"[..]));
    assert_eq!(
        fake.state().instances["loom-c1"].execs.last().unwrap(),
        &loom_host::stdio_exec().command
    );
    assert!(driver.stop("other", "loom-c1").await.is_err());
    driver.stop(ORACLE, "loom-c1").await.unwrap();
    assert!(!fake.state().instances["loom-c1"].instance.running());
    driver.stop(ORACLE, "loom-c1").await.unwrap();
    let err = loom_host::stdio(
        &driver,
        ORACLE,
        "c1",
        Box::new(tokio::io::empty()),
        Box::new(tokio::io::sink()),
        Box::new(tokio::io::sink()),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("stopped"), "{err}");
    driver.start(ORACLE, "loom-c1").await.unwrap();
    assert!(fake.state().instances["loom-c1"].instance.running());

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
    assert_eq!(driver.setup_project().await.unwrap(), SetupResult::Created);
    assert_eq!(
        driver.setup_project().await.unwrap(),
        SetupResult::Unchanged
    );
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
async fn network_rules_go_to_the_network_interface() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    let rules = HostRules {
        network: NetworkRules {
            isolate: false,
            acls: vec!["loom-codespaces".into(), "loom-extra".into()],
            default_egress: AclAction::Reject,
            default_ingress: AclAction::Drop,
        },
        ..HostRules::default()
    };
    let driver = IncusDriver::new(
        fake.client(Some("loom")),
        "default".into(),
        "incusbr0".into(),
        rules,
    );
    let binary = dir.path().join("loom-agent");
    std::fs::write(&binary, b"agent").unwrap();
    let agent = Agent {
        path: binary,
        version: None,
    };
    driver
        .create(
            &request("n1", Environment::default()),
            ORACLE,
            &agent,
            &Progress::none(),
        )
        .await
        .unwrap();
    {
        let state = fake.state();
        let nic = &state.instances["loom-n1"].instance.devices["eth0"];
        assert!(nic.get("security.port_isolation").is_none());
        assert_eq!(nic["security.acls"], "loom-codespaces,loom-extra");
        assert_eq!(nic["security.acls.default.egress.action"], "reject");
        assert_eq!(nic["security.acls.default.ingress.action"], "drop");
    }

    let network = |items: Vec<loom_host::CheckItem>| {
        items
            .into_iter()
            .find(|item| item.name == "network")
            .unwrap()
    };
    let item = network(driver.check(false).await);
    assert_eq!(item.status, CheckStatus::Failed, "{item:?}");
    assert!(
        item.detail.contains("loom-codespaces does not exist"),
        "{item:?}"
    );
    fake.state().acls = vec!["loom-codespaces".into(), "loom-extra".into()];
    let item = network(driver.check(false).await);
    assert_eq!(item.status, CheckStatus::Ok, "{item:?}");
    assert!(item.detail.contains("default egress reject"), "{item:?}");
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
        HostRules::default(),
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

#[tokio::test]
async fn custom_volumes_are_created_once_and_survive_instance_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    let driver = |rules| {
        IncusDriver::new(
            fake.client(Some("loom")),
            "default".into(),
            "incusbr0".into(),
            rules,
        )
    };
    assert_eq!(
        driver(HostRules::default()).setup_project().await.unwrap(),
        SetupResult::Created
    );
    let source = dir.path().canonicalize().unwrap().join("shared");
    std::fs::create_dir(&source).unwrap();
    let driver = driver(HostRules {
        directories: vec![source.clone()],
        host_user: Some(HostUser {
            uid: 1001,
            gid: 1002,
        }),
        ..Default::default()
    });
    assert!(
        matches!(driver.setup_project().await.unwrap(), SetupResult::Updated(keys) if keys.len() == 4)
    );
    assert_eq!(
        driver.setup_project().await.unwrap(),
        SetupResult::Unchanged
    );
    {
        let state = fake.state();
        let config = &state.projects["loom"].config;
        assert_eq!(config["features.storage.volumes"], "true");
        assert_eq!(config["restricted.devices.disk"], "allow");
        assert_eq!(
            config["restricted.devices.disk.paths"],
            source.to_str().unwrap()
        );
        assert_eq!(config["restricted.idmap.uid"], "1001");
        assert_eq!(config["restricted.idmap.gid"], "1002");
    }
    let binary = dir.path().join("loom-agent");
    std::fs::write(&binary, b"agent").unwrap();
    let agent = Agent {
        path: binary,
        version: None,
    };
    let progress = Progress::none();
    let mut first = request("c1", Environment::default());
    first.volumes = vec![
        InstanceVolume {
            name: "cache".into(),
            kind: VolumeKind::Incus,
            size_mib: Some(128),
            source: None,
            path: "/cache".into(),
            read_only: false,
        },
        InstanceVolume {
            name: "data".into(),
            kind: VolumeKind::Directory,
            size_mib: None,
            source: Some(source.to_str().unwrap().to_owned()),
            path: "/data".into(),
            read_only: true,
        },
    ];
    let mut second = request("c2", Environment::default());
    second.volumes = first.volumes.clone();
    second.volumes[0].read_only = true;
    let (a, b) = tokio::join!(
        driver.create(&first, ORACLE, &agent, &progress),
        driver.create(&second, ORACLE, &agent, &progress)
    );
    a.unwrap();
    b.unwrap();
    {
        let state = fake.state();
        let volume = &state.volumes["loom-cache"]["config"];
        for (key, value) in [
            (KEY_ORACLE, ORACLE),
            ("initial.uid", "1000"),
            ("initial.gid", "1000"),
            ("initial.mode", "0755"),
            ("size", "128MiB"),
        ] {
            assert_eq!(volume[key], value);
        }
        let first = &state.instances["loom-c1"].instance;
        assert_eq!(first.config["raw.idmap"], "uid 1001 1000\ngid 1002 1000");
        assert_eq!(first.devices["volume0"]["source"], "loom-cache");
        assert_eq!(first.devices["volume0"]["pool"], "default");
        assert!(!first.devices["volume0"].contains_key("readonly"));
        assert_eq!(first.devices["volume1"]["source"], source.to_str().unwrap());
        assert_eq!(first.devices["volume1"]["readonly"], "true");
        assert_eq!(
            state.instances["loom-c2"].instance.devices["volume0"]["readonly"],
            "true"
        );
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|r| r.as_str() == "POST /1.0/storage-pools/default/volumes/custom")
                .count(),
            1
        );
    }
    assert!(
        format!(
            "{:#}",
            driver.delete_volume(ORACLE, "cache").await.unwrap_err()
        )
        .contains("in use")
    );
    assert!(
        format!(
            "{:#}",
            driver.delete_volume("other", "cache").await.unwrap_err()
        )
        .contains("does not belong")
    );
    let usage = driver
        .volume_usage(ORACLE, &["cache".into(), "missing".into()])
        .await;
    assert_eq!(usage[0].used_bytes, Some(4096));
    assert!(usage[0].error.is_none());
    assert!(usage[1].error.is_some());
    assert!(
        driver.volume_usage("other", &["cache".into()]).await[0]
            .error
            .is_some()
    );
    let mut wrong_owner = request("c3", Environment::default());
    wrong_owner.volumes = vec![first.volumes[0].clone()];
    assert!(
        format!(
            "{:#}",
            driver
                .create(&wrong_owner, "other", &agent, &progress)
                .await
                .unwrap_err()
        )
        .contains("does not belong")
    );
    driver.delete(ORACLE, "loom-c1").await.unwrap();
    driver.delete(ORACLE, "loom-c2").await.unwrap();
    assert!(fake.state().volumes.contains_key("loom-cache"));
    driver.delete_volume(ORACLE, "cache").await.unwrap();
    driver.delete_volume(ORACLE, "cache").await.unwrap();
    assert!(fake.state().volumes.is_empty());
    assert!(source.is_dir());
}

#[tokio::test]
async fn session_stores_are_copied_from_a_stopped_instance() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    let sessions = "/home/dev/.local/share/loom/sessions";
    {
        let mut state = fake.state();
        let mut instance = FakeInstance::default();
        instance.instance.name = "loom-c1".into();
        instance.instance.status = "Stopped".into();
        instance
            .instance
            .config
            .insert(KEY_ORACLE.into(), ORACLE.into());
        for (name, content) in [
            ("0a1b.sqlite", b"database".as_slice()),
            ("0a1b.sqlite-wal", b"wal"),
            ("0a1b.sqlite-shm", b"shm"),
            ("notes.txt", b"other"),
        ] {
            instance.files.insert(
                format!("{sessions}/{name}"),
                FakeFile {
                    content: content.to_vec(),
                    kind: "file".into(),
                    ..Default::default()
                },
            );
        }
        state.instances.insert("loom-c1".into(), instance);
    }
    let driver = IncusDriver::new(
        fake.client(Some("loom")),
        "default".into(),
        "incusbr0".into(),
        HostRules::default(),
    );
    let target = dir.path().join("copy");
    std::fs::create_dir_all(&target).unwrap();
    driver
        .copy_sessions(ORACLE, "loom-c1", &target, 1024)
        .await
        .unwrap();
    let mut names: Vec<String> = std::fs::read_dir(&target)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["0a1b.sqlite", "0a1b.sqlite-wal"]);
    assert_eq!(
        std::fs::read(target.join("0a1b.sqlite")).unwrap(),
        b"database"
    );
    assert!(!fake.state().instances["loom-c1"].instance.running());

    let err = driver
        .copy_sessions(ORACLE, "loom-c1", &target, 10)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("more than"), "{err:#}");
    assert!(
        driver
            .copy_sessions("other", "loom-c1", &target, 1024)
            .await
            .is_err()
    );
    fake.state()
        .instances
        .get_mut("loom-c1")
        .unwrap()
        .files
        .clear();
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    driver
        .copy_sessions(ORACLE, "loom-c1", &empty, 1024)
        .await
        .unwrap();
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
}
