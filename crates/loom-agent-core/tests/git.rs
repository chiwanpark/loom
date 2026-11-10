use std::path::Path;
use std::process::Command;

use loom_agent_core::git::{changes, file_diff};
use loom_proto::{ChangeStatus, ErrorCode};

fn run(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(status.status.success(), "git {args:?} failed: {status:?}");
}

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["init", "-q"]);
    write(root, ".gitignore", "*.log\n");
    write(root, "a.txt", "one\ntwo\nthree\n");
    write(root, "b.txt", "gone\n");
    write(root, "old.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    write(root, "sub/inner.txt", "inner\n");
    run(root, &["add", "-A"]);
    run(root, &["commit", "-q", "-m", "init"]);
    dir
}

#[tokio::test]
async fn lists_modified_deleted_renamed_and_untracked_files() {
    let dir = repo();
    let root = dir.path();
    write(root, "a.txt", "one\n2\nthree\nfour\n");
    std::fs::remove_file(root.join("b.txt")).unwrap();
    run(root, &["mv", "old.txt", "new.txt"]);
    write(root, "c.txt", "x\ny\n");
    write(root, "debug.log", "ignored\n");

    let found = changes(root).await.unwrap();
    assert!(found.repository);
    assert_eq!(found.branch.as_deref(), Some("main"));
    assert!(!found.truncated);
    let summary: Vec<_> = found
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("a.txt", ChangeStatus::Modified, 2, 1),
            ("b.txt", ChangeStatus::Deleted, 0, 1),
            ("c.txt", ChangeStatus::Untracked, 2, 0),
            ("new.txt", ChangeStatus::Renamed, 0, 0),
        ]
    );
    assert_eq!(found.files[3].old_path.as_deref(), Some("old.txt"));

    let diff = file_diff(root, "a.txt", None).await.unwrap();
    assert!(diff.diff.contains("-two\n+2\n"), "{}", diff.diff);
    assert!(!diff.binary && !diff.truncated);

    let diff = file_diff(root, "c.txt", None).await.unwrap();
    assert!(diff.diff.contains("+x\n+y\n"), "{}", diff.diff);

    let diff = file_diff(root, "new.txt", Some("old.txt")).await.unwrap();
    assert!(diff.diff.contains("rename from old.txt"), "{}", diff.diff);

    let err = file_diff(root, "../outside", None).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
}

#[tokio::test]
async fn limits_changes_to_a_workspace_inside_the_repository() {
    let dir = repo();
    let root = dir.path();
    write(root, "a.txt", "changed\n");
    write(root, "sub/inner.txt", "inner\nmore\n");
    write(root, "sub/fresh.txt", "fresh\n");

    let sub = root.join("sub");
    let found = changes(&sub).await.unwrap();
    let paths: Vec<_> = found.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["fresh.txt", "inner.txt"]);
    let diff = file_diff(&sub, "inner.txt", None).await.unwrap();
    assert!(diff.diff.contains("+more\n"), "{}", diff.diff);
}

#[tokio::test]
async fn handles_repositories_without_commits_and_plain_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let plain = changes(root).await.unwrap();
    assert!(!plain.repository);
    assert!(plain.files.is_empty());
    let err = file_diff(root, "a.txt", None).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Unsupported);

    run(root, &["init", "-q"]);
    write(root, "staged.txt", "s\n");
    write(root, "loose.bin", "a\0b");
    run(root, &["add", "staged.txt"]);
    let found = changes(root).await.unwrap();
    assert!(found.repository);
    let summary: Vec<_> = found
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.binary))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("loose.bin", ChangeStatus::Untracked, true),
            ("staged.txt", ChangeStatus::Added, false),
        ]
    );
    let diff = file_diff(root, "loose.bin", None).await.unwrap();
    assert!(diff.binary, "{}", diff.diff);
}
