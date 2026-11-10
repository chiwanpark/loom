use loom_agent_core::files::{list_directory, read_file};
use loom_proto::{EntryKind, ErrorCode};

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
