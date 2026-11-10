use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use loom_transport::{
    AgentEntry, MAX_FRAME_LEN, PeerInfo, Registry, RegistryError, UnixFrameListener,
    connect_command, connect_unix, current_uid, memory_pair,
};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn memory_pair_carries_frames_both_ways() {
    let (mut a, mut b) = memory_pair();
    let big = Bytes::from(vec![7u8; 1024 * 1024]);
    let expected = big.clone();
    let sender = tokio::spawn(async move {
        a.send(Bytes::from_static(b"hello")).await.unwrap();
        a.send(Bytes::new()).await.unwrap();
        a.send(big).await.unwrap();
        a
    });
    assert_eq!(b.recv().await.unwrap().unwrap(), "hello");
    assert_eq!(b.recv().await.unwrap().unwrap(), "");
    assert_eq!(b.recv().await.unwrap().unwrap(), expected);
    let mut a = sender.await.unwrap();
    b.send(Bytes::from_static(b"back")).await.unwrap();
    assert_eq!(a.recv().await.unwrap().unwrap(), "back");
    drop(a);
    assert!(b.recv().await.unwrap().is_none());
}

#[tokio::test]
async fn oversized_frames_are_rejected() {
    let (mut a, _b) = memory_pair();
    let err = a
        .send(Bytes::from(vec![0u8; MAX_FRAME_LEN + 1]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    let (raw_client, raw_server) = tokio::io::duplex(1024);
    let (read, _write) = tokio::io::split(raw_server);
    let (_r, mut raw_write) = tokio::io::split(raw_client);
    let mut stream = loom_transport::FrameStream::new(read, tokio::io::sink(), PeerInfo::Memory);
    raw_write
        .write_all(&(u32::MAX).to_be_bytes())
        .await
        .unwrap();
    assert!(stream.recv().await.is_err());
}

#[tokio::test]
async fn split_halves_work_concurrently() {
    let (a, mut b) = memory_pair();
    let (mut tx, mut rx, peer) = a.split();
    assert_eq!(peer, PeerInfo::Memory);
    let echo = tokio::spawn(async move {
        while let Some(frame) = b.recv().await.unwrap() {
            b.send(frame).await.unwrap();
        }
    });
    let sender = tokio::spawn(async move {
        for i in 0..100u32 {
            tx.send(Bytes::from(i.to_be_bytes().to_vec()))
                .await
                .unwrap();
        }
        tx.close().await.unwrap();
    });
    for i in 0..100u32 {
        let frame = rx.recv().await.unwrap().unwrap();
        assert_eq!(frame.as_ref(), i.to_be_bytes());
    }
    sender.await.unwrap();
    echo.await.unwrap();
}

#[tokio::test]
async fn unix_socket_checks_peer_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.sock");
    let listener = UnixFrameListener::bind(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        assert!(matches!(stream.peer(), PeerInfo::Local { uid, .. } if *uid == current_uid()));
        let frame = stream.recv().await.unwrap().unwrap();
        stream.send(frame).await.unwrap();
        listener
    });
    let mut client = connect_unix(&path).await.unwrap();
    client.send(Bytes::from_static(b"ping")).await.unwrap();
    assert_eq!(client.recv().await.unwrap().unwrap(), "ping");
    let listener = server.await.unwrap();
    drop(listener);
    assert!(!path.exists());

    std::fs::write(&path, "not a socket").unwrap();
    assert!(UnixFrameListener::bind(&path).is_err());
    let long = dir.path().join("x".repeat(120));
    assert!(UnixFrameListener::bind(&long).is_err());
}

#[tokio::test]
async fn command_transport_frames_child_stdio() {
    let mut stream = connect_command(tokio::process::Command::new("cat")).unwrap();
    assert!(matches!(stream.peer(), PeerInfo::Command { program } if program == "cat"));
    stream.send(Bytes::from_static(b"echo me")).await.unwrap();
    assert_eq!(stream.recv().await.unwrap().unwrap(), "echo me");

    let mut command = tokio::process::Command::new("sh");
    command.args(["-c", "echo 'remote: no such command' >&2; exit 127"]);
    let mut failing = connect_command(command).unwrap();
    assert!(failing.recv().await.unwrap().is_none());
    let diagnostics = failing.diagnostics().unwrap();
    for _ in 0..50 {
        if !diagnostics.text().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(diagnostics.text().contains("no such command"));
}

fn entry(registry: &Registry, instance: &str, workspace: &Path) -> AgentEntry {
    AgentEntry {
        instance: instance.into(),
        pid: std::process::id(),
        workspace: workspace.into(),
        socket: registry.socket_path(instance),
        started_at: 1,
        version: "test".into(),
    }
}

#[tokio::test]
async fn registry_tracks_live_agents_and_one_per_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("loom"));
    let ws = Path::new("/work/a");
    assert!(registry.list().unwrap().is_empty());

    let mut first = registry.claim("i1", ws).unwrap();
    assert!(registry.list().unwrap().is_empty());
    first.publish(entry(&registry, "i1", ws)).unwrap();
    let mode = std::fs::metadata(registry.root()).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o700
    );
    assert!(matches!(
        registry.claim("i2", ws),
        Err(RegistryError::AlreadyRunning(_))
    ));
    let mut other = registry.claim("i3", Path::new("/work/b")).unwrap();
    other
        .publish(entry(&registry, "i3", Path::new("/work/b")))
        .unwrap();

    let listed = registry.list().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(registry.find_workspace(ws).unwrap().unwrap().instance, "i1");
    assert_eq!(
        registry.find_instance("i3").unwrap().unwrap().workspace,
        Path::new("/work/b")
    );

    drop(first);
    assert!(registry.find_workspace(ws).unwrap().is_none());
    let mut again = registry.claim("i4", ws).unwrap();
    again.publish(entry(&registry, "i4", ws)).unwrap();
    assert_eq!(
        registry
            .wait_for_workspace(ws, Duration::from_secs(1), || None)
            .await
            .unwrap()
            .instance,
        "i4"
    );
    assert!(
        !registry
            .wait_until_gone("i4", Duration::from_millis(100))
            .await
            .unwrap()
    );
    drop(again);
    assert!(
        registry
            .wait_until_gone("i4", Duration::from_millis(100))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn registry_removes_stale_entries() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path());
    registry.ensure_dirs().unwrap();
    let stale = entry(&registry, "dead", Path::new("/work/x"));
    let agents = dir.path().join("agents");
    std::fs::write(
        agents.join("dead.json"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    std::fs::write(agents.join("dead.lock"), "").unwrap();
    assert!(registry.list().unwrap().is_empty());
    assert!(!agents.join("dead.json").exists());
    assert!(!agents.join("dead.lock").exists());
    let err = registry
        .wait_for_workspace(Path::new("/work/x"), Duration::from_millis(200), || {
            Some("child exited".into())
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("child exited"));
}
