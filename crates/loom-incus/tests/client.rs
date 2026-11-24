use std::collections::BTreeMap;
use std::time::Duration;

use loom_incus::fake::FakeIncus;
use loom_incus::{Exec, FileOwner, Project, StateAction};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn instances_go_through_their_life() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    let incus = fake.client(Some("loom"));
    assert!(incus.is_local());
    let server = incus.server().await.unwrap();
    assert_eq!(server.auth, "trusted");
    assert_eq!(server.environment.project, "loom");
    assert_eq!(incus.resources().await.unwrap().cpu.total, 8);
    assert_eq!(incus.storage_pool("default").await.unwrap().driver, "dir");
    assert!(incus.storage_pool("none").await.unwrap_err().is_not_found());
    assert!(incus.network("incusbr0").await.is_ok());

    assert!(incus.project_info("loom").await.unwrap_err().is_not_found());
    incus
        .create_project(&Project {
            name: "loom".into(),
            description: String::new(),
            config: BTreeMap::from([("restricted".into(), "true".into())]),
        })
        .await
        .unwrap();
    assert_eq!(
        incus.project_info("loom").await.unwrap().config["restricted"],
        "true"
    );

    let body = json!({
        "name": "loom-c1",
        "type": "container",
        "source": {"type": "image", "alias": "debian/13", "server": "https://images.linuxcontainers.org", "protocol": "simplestreams"},
        "config": {"user.loom.codespace": "c1"},
        "devices": {},
        "profiles": [],
    });
    incus.create_instance(&body, WAIT).await.unwrap();
    let err = incus.create_instance(&body, WAIT).await.unwrap_err();
    assert!(err.message().contains("already exists"), "{err}");
    let instance = incus.instance("loom-c1").await.unwrap().unwrap();
    assert_eq!(instance.config["user.loom.codespace"], "c1");
    assert!(!instance.running());
    assert!(incus.instance("loom-none").await.unwrap().is_none());

    incus
        .push_file(
            "loom-c1",
            "/home/dev/.config/loom/boot strap.json",
            b"{}".to_vec(),
            FileOwner {
                uid: 1000,
                gid: 1000,
                mode: 0o600,
            },
        )
        .await
        .unwrap();
    incus
        .push_symlink("loom-c1", "/etc/x.service", "/lib/x.service")
        .await
        .unwrap();
    incus
        .push_directory("loom-c1", "/workspaces", FileOwner::root(0o755))
        .await
        .unwrap();
    {
        let state = fake.state();
        let files = &state.instances["loom-c1"].files;
        let file = &files["/home/dev/.config/loom/boot strap.json"];
        assert_eq!(
            (file.uid, file.gid, file.mode.as_str()),
            (1000, 1000, "0600")
        );
        assert_eq!(file.kind, "file");
        assert_eq!(files["/etc/x.service"].content, b"/lib/x.service");
        assert_eq!(files["/etc/x.service"].kind, "symlink");
        assert_eq!(files["/workspaces"].kind, "directory");
    }

    assert!(
        incus
            .exec("loom-c1", &Exec::new(["true"]), WAIT)
            .await
            .is_err()
    );
    incus
        .set_state("loom-c1", StateAction::Start, false)
        .await
        .unwrap();
    assert_eq!(
        incus
            .instance_state("loom-c1")
            .await
            .unwrap()
            .global_ipv4()
            .as_deref(),
        Some("10.1.2.3")
    );
    fake.state().exec = Some(Box::new(|_, command| {
        (
            if command[0] == "false" { 1 } else { 0 },
            format!("ran {}\n", command.join(" ")),
            "warn\n".into(),
        )
    }));
    let output = incus
        .exec("loom-c1", &Exec::new(["echo", "hi"]), WAIT)
        .await
        .unwrap();
    assert!(output.success());
    assert_eq!(output.stdout, b"ran echo hi\n");
    assert_eq!(output.stderr, b"warn\n");
    let failed = incus
        .exec("loom-c1", &Exec::new(["false"]), WAIT)
        .await
        .unwrap();
    assert_eq!(failed.code, 1);

    let input: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = incus
        .exec_stream(
            "loom-c1",
            &Exec::new(["cat"]),
            input.as_slice(),
            &mut stdout,
            &mut stderr,
        )
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(stdout, input);
    assert!(stderr.is_empty());
    fake.state().stream = Some(Box::new(|_, command, stdin| {
        (7, stdin.to_vec(), command.join(" ").into_bytes())
    }));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = incus
        .exec_stream(
            "loom-c1",
            &Exec::new(["sh", "-c", "x"]),
            &b"hi"[..],
            &mut stdout,
            &mut stderr,
        )
        .await
        .unwrap();
    assert_eq!(
        (code, stdout.as_slice(), stderr.as_slice()),
        (7, &b"hi"[..], &b"sh -c x"[..])
    );

    assert!(incus.delete_instance("loom-c1").await.is_err());
    incus
        .set_state("loom-c1", StateAction::Stop, true)
        .await
        .unwrap();
    let fingerprint = incus
        .publish(
            "loom-c1",
            "loom-image-x",
            &BTreeMap::new(),
            Some("zstd"),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(
        incus.image_alias("loom-image-x").await.unwrap().as_deref(),
        Some(fingerprint.as_str())
    );
    assert!(incus.image_alias("nothing").await.unwrap().is_none());
    fake.state().reject_zstd = true;
    let err = incus
        .publish(
            "loom-c1",
            "loom-image-y",
            &BTreeMap::new(),
            Some("zstd"),
            WAIT,
        )
        .await
        .unwrap_err();
    assert!(err.message().contains("zstd"), "{err}");
    assert_eq!(incus.images().await.unwrap().len(), 1);
    incus.delete_image(&fingerprint).await.unwrap();
    incus.delete_instance("loom-c1").await.unwrap();
    assert!(incus.instances().await.unwrap().is_empty());
    assert!(
        fake.state()
            .requests
            .iter()
            .any(|line| line == "GET /1.0/instances?recursion=1")
    );
}

#[tokio::test]
async fn certificates_register_with_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeIncus::start(dir.path()).await;
    fake.client(None).add_certificate(" token\n").await.unwrap();
    assert_eq!(fake.state().certificates, vec!["token".to_owned()]);
}
