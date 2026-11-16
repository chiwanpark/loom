use std::fmt::Write;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn stub(name: &str, version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho '{name} {version} (stub)'\n").into_bytes()
}

/// A release archive holding `files` in its directory.
fn archive(stem: &str, files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, format!("{stem}/{name}"), data.as_slice())
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

fn sha256(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Serves a release with `archives` at `dir` under the server, such as
/// `latest/download` or `download/v0.4.0`.
async fn serve(server: &MockServer, dir: &str, archives: &[(String, Vec<u8>)]) {
    let mut sums = String::new();
    for (name, data) in archives {
        writeln!(sums, "{}  {name}", sha256(data)).unwrap();
        Mock::given(method("GET"))
            .and(path(format!("/{dir}/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
            .mount(server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(format!("/{dir}/SHA256SUMS")))
        .respond_with(ResponseTemplate::new(200).set_body_string(sums))
        .mount(server)
        .await;
}

fn agent_archive(version: &str) -> (String, Vec<u8>) {
    let target = Package::Agent.target().unwrap();
    let stem = format!("loom-agent-{version}-{target}");
    let data = archive(
        &stem,
        &[
            ("loom-agent", stub("loom-agent", version)),
            ("README.md", b"readme".to_vec()),
        ],
    );
    (format!("{stem}.tar.gz"), data)
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn archives_are_found_in_the_checksums() {
    let hash = "a".repeat(64);
    let sums = format!(
        "{hash}  loom-0.4.0-x86_64-unknown-linux-gnu.tar.gz\n\
         {hash}  loom-agent-0.4.0-aarch64-unknown-linux-musl.tar.gz\n\
         {hash} *loom-agent-0.4.0-x86_64-unknown-linux-musl.tar.gz\n"
    );
    let (_, file, version) =
        find_archive(&sums, "loom-agent", "x86_64-unknown-linux-musl").unwrap();
    assert_eq!(file, "loom-agent-0.4.0-x86_64-unknown-linux-musl.tar.gz");
    assert_eq!(version, Version::new(0, 4, 0));
    let (_, file, _) = find_archive(&sums, "loom", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(file, "loom-0.4.0-x86_64-unknown-linux-gnu.tar.gz");
    assert!(find_archive(&sums, "loom", "x86_64-unknown-linux-musl").is_none());
    assert!(find_archive(&sums, "loom-relay", "x86_64-unknown-linux-musl").is_none());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[tokio::test]
async fn the_latest_release_replaces_the_binaries() {
    let server = MockServer::start().await;
    let target = Package::Desktop.target().unwrap();
    let stem = format!("loom-0.9.0-{target}");
    let desktop = (
        format!("{stem}.tar.gz"),
        archive(
            &stem,
            &[
                ("loom", stub("loom", "0.9.0")),
                ("loom-agent", stub("loom-agent", "0.9.0")),
            ],
        ),
    );
    serve(
        &server,
        "latest/download",
        &[desktop, agent_archive("0.9.0")],
    )
    .await;
    let updater = Updater::with_releases_url(server.uri());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("loom"), stub("loom", "0.1.0")).unwrap();
    std::fs::set_permissions(
        dir.path().join("loom"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let release = updater.release(Package::Desktop, None).await.unwrap();
    assert_eq!(release.version, Version::new(0, 9, 0));
    let mut seen = Vec::new();
    let installed = updater
        .install(Package::Desktop, &release, dir.path(), |done, total| {
            seen.push((done, total))
        })
        .await
        .unwrap();
    assert_eq!(
        installed,
        vec![
            Installed {
                binary: "loom",
                path: dir.path().join("loom"),
                old: Some("loom 0.1.0 (stub)".into()),
                new: "loom 0.9.0 (stub)".into(),
            },
            Installed {
                binary: "loom-agent",
                path: dir.path().join("loom-agent"),
                old: None,
                new: "loom-agent 0.9.0 (stub)".into(),
            },
        ]
    );
    assert_eq!(
        std::fs::read(dir.path().join("loom-agent")).unwrap(),
        stub("loom-agent", "0.9.0")
    );
    assert_eq!(files(dir.path()), ["loom", "loom-agent"]);
    assert!(seen.last().is_some_and(|(done, _)| *done > 0));
}

#[tokio::test]
async fn a_release_of_another_version_is_found() {
    let server = MockServer::start().await;
    serve(&server, "download/v0.2.0", &[agent_archive("0.2.0")]).await;
    let updater = Updater::with_releases_url(server.uri());
    let Ok(target) = Package::Agent.target() else {
        return;
    };
    let release = updater
        .release(Package::Agent, Some(&Version::new(0, 2, 0)))
        .await
        .unwrap();
    assert_eq!(release.archive, format!("loom-agent-0.2.0-{target}.tar.gz"));
    let err = updater
        .release(Package::Agent, Some(&Version::new(0, 5, 0)))
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Download { .. }), "{err}");
}

#[tokio::test]
async fn a_corrupt_archive_changes_nothing() {
    if Package::Agent.target().is_err() {
        return;
    }
    let server = MockServer::start().await;
    let (name, data) = agent_archive("0.9.0");
    let sums = format!("{}  {name}\n", sha256(b"something else"));
    Mock::given(method("GET"))
        .and(path("/latest/download/SHA256SUMS"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sums))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/latest/download/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data))
        .mount(&server)
        .await;
    let updater = Updater::with_releases_url(server.uri());
    let dir = tempfile::tempdir().unwrap();
    let release = updater.release(Package::Agent, None).await.unwrap();
    let err = updater
        .install(Package::Agent, &release, dir.path(), |_, _| {})
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Checksum { .. }), "{err}");
    assert!(files(dir.path()).is_empty());
}

#[tokio::test]
async fn a_binary_that_does_not_run_changes_nothing() {
    let Ok(target) = Package::Agent.target() else {
        return;
    };
    let server = MockServer::start().await;
    let stem = format!("loom-agent-0.9.0-{target}");
    let broken = (
        format!("{stem}.tar.gz"),
        archive(&stem, &[("loom-agent", b"#!/bin/sh\nexit 3\n".to_vec())]),
    );
    serve(&server, "latest/download", &[broken]).await;
    let updater = Updater::with_releases_url(server.uri());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("loom-agent"), "old").unwrap();
    let release = updater.release(Package::Agent, None).await.unwrap();
    let err = updater
        .install(Package::Agent, &release, dir.path(), |_, _| {})
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Broken { .. }), "{err}");
    assert_eq!(files(dir.path()), ["loom-agent"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("loom-agent")).unwrap(),
        "old"
    );
}

#[tokio::test]
async fn an_archive_without_the_binary_is_refused() {
    let Ok(target) = Package::Agent.target() else {
        return;
    };
    let server = MockServer::start().await;
    let stem = format!("loom-agent-0.9.0-{target}");
    let empty = (
        format!("{stem}.tar.gz"),
        archive(&stem, &[("README.md", b"readme".to_vec())]),
    );
    serve(&server, "latest/download", &[empty]).await;
    let updater = Updater::with_releases_url(server.uri());
    let dir = tempfile::tempdir().unwrap();
    let release = updater.release(Package::Agent, None).await.unwrap();
    let err = updater
        .install(Package::Agent, &release, dir.path(), |_, _| {})
        .await
        .unwrap_err();
    assert!(err.to_string().ends_with("has no loom-agent"), "{err}");
    assert!(files(dir.path()).is_empty());
}
