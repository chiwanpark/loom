use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use loom_agent_core::files::{list_directory, read_file, write_file};
use loom_proto::{EntryKind, ErrorCode, FileVersion};
use loom_tools::{FileAccess, Workspace};

fn names(listing: &loom_proto::DirectoryListing) -> Vec<(&str, EntryKind, bool)> {
    listing
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind, e.ignored))
        .collect()
}

#[tokio::test]
async fn lists_directories_first_and_hides_the_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src/nested")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("b.txt"), "b").unwrap();
    std::fs::write(root.join("A.md"), "a").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

    let top = list_directory(&root, "").await.unwrap();
    assert_eq!(
        names(&top),
        vec![
            ("src", EntryKind::Directory, false),
            ("A.md", EntryKind::File, false),
            ("b.txt", EntryKind::File, false),
        ]
    );
    let src = list_directory(&root, "src").await.unwrap();
    assert_eq!(src.path, "src");
    assert_eq!(
        names(&src),
        vec![
            ("nested", EntryKind::Directory, false),
            ("main.rs", EntryKind::File, false),
        ]
    );
    assert_eq!(src.entries[1].size, 13);
}

#[tokio::test]
async fn marks_gitignored_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::create_dir_all(root.join("logs")).unwrap();
    std::fs::write(root.join("logs/run.log"), "x").unwrap();
    std::fs::write(root.join("logs/keep.txt"), "x").unwrap();

    let top = list_directory(&root, "").await.unwrap();
    assert_eq!(
        names(&top),
        vec![
            ("logs", EntryKind::Directory, false),
            ("target", EntryKind::Directory, true),
            (".gitignore", EntryKind::File, false),
        ]
    );
    let logs = list_directory(&root, "logs").await.unwrap();
    assert_eq!(
        names(&logs),
        vec![
            ("keep.txt", EntryKind::File, false),
            ("run.log", EntryKind::File, true),
        ]
    );
}

#[tokio::test]
async fn reads_text_and_refuses_paths_outside_the_workspace() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("notes.md"), "# Notes\n").unwrap();
    std::fs::write(root.join("blob.bin"), b"a\0b").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();

    let notes = read_file(&root, "notes.md").await.unwrap();
    assert_eq!(notes.text, "# Notes\n");
    assert_eq!(notes.size, 8);
    assert!(!notes.binary && !notes.truncated);
    let blob = read_file(&root, "blob.bin").await.unwrap();
    assert!(blob.binary);
    assert!(blob.text.is_empty());

    for path in ["../x", "/etc/passwd", "escape/secret.txt"] {
        let err = read_file(&root, path).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest, "{path}");
    }
    let err = list_directory(&root, "escape").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
    let err = read_file(&root, "missing.txt").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn truncates_large_files_at_a_line_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let line = "0123456789abcdef\n";
    std::fs::write(root.join("big.txt"), line.repeat(200_000)).unwrap();
    let big = read_file(&root, "big.txt").await.unwrap();
    assert!(big.truncated);
    assert_eq!(big.size, (line.len() * 200_000) as u64);
    assert!(big.text.ends_with('\n'));
    assert!(big.text.len() <= 2 * 1024 * 1024);
}

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path().canonicalize().unwrap()).unwrap();
    (dir, workspace)
}

#[tokio::test]
async fn writes_a_file_it_read_and_keeps_its_permissions() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("run.sh");
    std::fs::write(&path, "echo old\r\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();

    let read = read_file(workspace.root(), "run.sh").await.unwrap();
    assert!(read.valid_utf8);
    let version = write_file(&workspace, "run.sh", "echo new\r\n".into(), read.version)
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo new\r\n");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
    let reread = read_file(workspace.root(), "run.sh").await.unwrap();
    assert_eq!(reread.version, version);
    assert_ne!(version, read.version);
    // The returned version is good for the next save.
    write_file(&workspace, "run.sh", "echo newer\n".into(), version)
        .await
        .unwrap();
}

#[tokio::test]
async fn refuses_a_write_when_the_file_changed_since_it_was_read() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("a.txt");
    std::fs::write(&path, "one").unwrap();
    let read = read_file(workspace.root(), "a.txt").await.unwrap();
    std::fs::write(&path, "two").unwrap();

    let err = write_file(&workspace, "a.txt", "mine".into(), read.version)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
}

#[tokio::test]
async fn a_truncated_read_cannot_be_written_back() {
    let (_dir, workspace) = workspace();
    let line = "0123456789abcdef\n";
    let path = workspace.root().join("big.txt");
    std::fs::write(&path, line.repeat(200_000)).unwrap();
    let read = read_file(workspace.root(), "big.txt").await.unwrap();
    assert!(read.truncated);

    let err = write_file(&workspace, "big.txt", read.text, read.version)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        (line.len() * 200_000) as u64
    );
}

#[tokio::test]
async fn refuses_writes_that_are_not_edits_of_a_workspace_file() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let (_dir, workspace) = workspace();
    let root = workspace.root().to_path_buf();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("small.txt"), "x").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("escape")).unwrap();
    let any = FileVersion::default();

    for (path, code) in [
        ("../x", ErrorCode::BadRequest),
        ("/etc/passwd", ErrorCode::BadRequest),
        ("escape/secret.txt", ErrorCode::BadRequest),
        ("src", ErrorCode::BadRequest),
        ("missing.txt", ErrorCode::NotFound),
    ] {
        let err = write_file(&workspace, path, "x".into(), any)
            .await
            .unwrap_err();
        assert_eq!(err.code, code, "{path}");
    }
    let huge = "x".repeat(2 * 1024 * 1024 + 1);
    let err = write_file(&workspace, "small.txt", huge, any)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadRequest);
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "secret"
    );
    assert!(!root.join("missing.txt").exists());
}

#[tokio::test]
async fn reports_text_that_is_not_utf8() {
    let (_dir, workspace) = workspace();
    std::fs::write(workspace.root().join("latin1.txt"), b"caf\xe9\n").unwrap();
    let read = read_file(workspace.root(), "latin1.txt").await.unwrap();
    assert!(!read.binary);
    assert!(!read.valid_utf8);
}

#[tokio::test]
async fn a_write_waits_for_a_tool_call_on_the_same_file() {
    let (_dir, workspace) = workspace();
    let workspace = std::sync::Arc::new(workspace);
    let path = workspace.root().join("a.txt");
    std::fs::write(&path, "one").unwrap();
    let read = read_file(workspace.root(), "a.txt").await.unwrap();

    let guard = workspace.locks().lock_file(&path, FileAccess::Write).await;
    let write = tokio::spawn({
        let workspace = workspace.clone();
        async move { write_file(&workspace, "a.txt", "two".into(), read.version).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!write.is_finished());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
    drop(guard);
    write.await.unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
}
