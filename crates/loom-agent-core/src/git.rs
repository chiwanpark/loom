use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use loom_proto::{ApiError, ChangeStatus, ErrorCode, FileChange, FileDiff, WorkspaceChanges};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_LISTING: usize = 8 * 1024 * 1024;
const MAX_FILES: usize = 2000;
const MAX_DIFF: usize = 1024 * 1024;
const MAX_COUNTED_BYTES: u64 = 4 * 1024 * 1024;
const BINARY_SNIFF: usize = 8000;
const PINNED_CONFIG: [&str; 10] = [
    "-c",
    "core.quotePath=false",
    "-c",
    "diff.noprefix=false",
    "-c",
    "diff.mnemonicPrefix=false",
    "-c",
    "diff.relative=false",
    "-c",
    "color.ui=never",
];

struct Output {
    status: i32,
    stdout: Vec<u8>,
    truncated: bool,
}

fn internal(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Internal, message)
}

async fn git(root: &Path, args: &[&str], cap: usize) -> Result<Output, ApiError> {
    let mut child = Command::new("git")
        .args(PINNED_CONFIG)
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| internal(format!("cannot run git: {err}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let run = async {
        let mut data = Vec::new();
        let mut reader = (&mut stdout).take(cap as u64 + 1);
        reader
            .read_to_end(&mut data)
            .await
            .map_err(|err| internal(format!("cannot read git output: {err}")))?;
        let truncated = data.len() > cap;
        if truncated {
            data.truncate(cap);
            let _ = child.start_kill();
        }
        let status = child
            .wait()
            .await
            .map_err(|err| internal(format!("git failed: {err}")))?;
        Ok(Output {
            status: if truncated {
                0
            } else {
                status.code().unwrap_or(-1)
            },
            stdout: data,
            truncated,
        })
    };
    tokio::time::timeout(GIT_TIMEOUT, run)
        .await
        .map_err(|_| internal("git timed out"))?
}

async fn git_ok(root: &Path, args: &[&str], cap: usize) -> Result<Output, ApiError> {
    let output = git(root, args, cap).await?;
    if output.status != 0 {
        return Err(internal(format!(
            "git {} exited with status {}",
            args.first().copied().unwrap_or_default(),
            output.status
        )));
    }
    Ok(output)
}

async fn is_repository(root: &Path) -> bool {
    match git(root, &["rev-parse", "--is-inside-work-tree"], 64).await {
        Ok(output) => output.status == 0 && output.stdout.starts_with(b"true"),
        Err(_) => false,
    }
}

async fn base(root: &Path) -> &'static str {
    match git(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], 256).await {
        Ok(output) if output.status == 0 => "HEAD",
        _ => EMPTY_TREE,
    }
}

async fn branch(root: &Path) -> Option<String> {
    let text = |output: Output| {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status == 0 && !name.is_empty()).then_some(name)
    };
    if let Some(name) = git(root, &["symbolic-ref", "--short", "-q", "HEAD"], 1024)
        .await
        .ok()
        .and_then(text)
    {
        return Some(name);
    }
    git(root, &["rev-parse", "--short", "HEAD"], 1024)
        .await
        .ok()
        .and_then(text)
        .map(|sha| format!("detached at {sha}"))
}

fn fields(data: &[u8]) -> impl Iterator<Item = String> + '_ {
    data.split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
}

fn status_of(code: &str) -> ChangeStatus {
    match code.chars().next() {
        Some('A') => ChangeStatus::Added,
        Some('D') => ChangeStatus::Deleted,
        Some('R') => ChangeStatus::Renamed,
        Some('C') => ChangeStatus::Copied,
        Some('T') => ChangeStatus::TypeChanged,
        Some('U') => ChangeStatus::Conflicted,
        _ => ChangeStatus::Modified,
    }
}

fn parse_name_status(data: &[u8]) -> Vec<FileChange> {
    let mut out = Vec::new();
    let mut it = fields(data);
    while let Some(code) = it.next() {
        let status = status_of(&code);
        let two_paths = matches!(status, ChangeStatus::Renamed | ChangeStatus::Copied);
        let first = match it.next() {
            Some(path) => path,
            None => break,
        };
        let (path, old_path) = if two_paths {
            match it.next() {
                Some(path) => (path, Some(first)),
                None => break,
            }
        } else {
            (first, None)
        };
        out.push(FileChange {
            path,
            old_path,
            status,
            added: 0,
            removed: 0,
            binary: false,
        });
    }
    out
}

fn parse_numstat(data: &[u8]) -> BTreeMap<String, (u32, u32, bool)> {
    let mut out = BTreeMap::new();
    let mut parts = data.split(|b| *b == 0).peekable();
    while let Some(head) = parts.next() {
        if head.is_empty() {
            continue;
        }
        let head = String::from_utf8_lossy(head);
        let mut cols = head.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = parts.next();
            match parts.next() {
                Some(new) => String::from_utf8_lossy(new).into_owned(),
                None => break,
            }
        } else {
            path.to_string()
        };
        let binary = added == "-" || removed == "-";
        out.insert(
            path,
            (
                added.parse().unwrap_or(0),
                removed.parse().unwrap_or(0),
                binary,
            ),
        );
    }
    out
}

fn count_untracked(path: &Path) -> (u32, bool) {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return (0, false);
    };
    let mut data = Vec::new();
    if file.take(MAX_COUNTED_BYTES).read_to_end(&mut data).is_err() {
        return (0, false);
    }
    if data[..data.len().min(BINARY_SNIFF)].contains(&0) {
        return (0, true);
    }
    let mut lines = data.iter().filter(|b| **b == b'\n').count();
    if data.last().is_some_and(|b| *b != b'\n') {
        lines += 1;
    }
    (u32::try_from(lines).unwrap_or(u32::MAX), false)
}

pub async fn changes(root: &Path) -> Result<WorkspaceChanges, ApiError> {
    if !is_repository(root).await {
        return Ok(WorkspaceChanges::default());
    }
    let base = base(root).await;
    let branch = branch(root).await;
    let diff_args = [
        "diff",
        base,
        "--relative",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-M",
        "-z",
    ];
    let mut args = diff_args.to_vec();
    args.push("--name-status");
    let names = git_ok(root, &args, MAX_LISTING).await?;
    let mut args = diff_args.to_vec();
    args.push("--numstat");
    let stats = git_ok(root, &args, MAX_LISTING).await?;
    let untracked = git_ok(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        MAX_LISTING,
    )
    .await?;
    let mut truncated = names.truncated || stats.truncated || untracked.truncated;

    let stats = parse_numstat(&stats.stdout);
    let mut files = parse_name_status(&names.stdout);
    for file in &mut files {
        if let Some((added, removed, binary)) = stats.get(&file.path) {
            file.added = *added;
            file.removed = *removed;
            file.binary = *binary;
        }
    }
    let untracked: Vec<String> = fields(&untracked.stdout).collect();
    let budget = MAX_FILES.saturating_sub(files.len());
    if untracked.len() > budget {
        truncated = true;
    }
    let paths: Vec<String> = untracked.into_iter().take(budget).collect();
    let root_owned = root.to_path_buf();
    let counted = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .map(|path| {
                let (added, binary) = count_untracked(&root_owned.join(&path));
                FileChange {
                    path,
                    old_path: None,
                    status: ChangeStatus::Untracked,
                    added,
                    removed: 0,
                    binary,
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|err| internal(format!("counting untracked files failed: {err}")))?;
    files.extend(counted);
    if files.len() > MAX_FILES {
        files.truncate(MAX_FILES);
        truncated = true;
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WorkspaceChanges {
        repository: true,
        branch,
        files,
        truncated,
    })
}

fn check_path(path: &str) -> Result<(), ApiError> {
    let bad = || ApiError::new(ErrorCode::BadRequest, format!("invalid path {path:?}"));
    if path.is_empty() || path.contains('\0') {
        return Err(bad());
    }
    let parsed = Path::new(path);
    if parsed
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(bad());
    }
    Ok(())
}

fn literal(path: &str) -> String {
    format!(":(literal){path}")
}

fn cut(mut data: Vec<u8>, truncated: bool) -> (String, bool) {
    let mut truncated = truncated;
    if data.len() > MAX_DIFF {
        data.truncate(MAX_DIFF);
        truncated = true;
    }
    if truncated && let Some(end) = data.iter().rposition(|b| *b == b'\n') {
        data.truncate(end + 1);
    }
    (String::from_utf8_lossy(&data).into_owned(), truncated)
}

fn is_binary_diff(diff: &str) -> bool {
    diff.lines()
        .any(|line| line.starts_with("Binary files ") && line.ends_with(" differ"))
}

pub async fn file_diff(
    root: &Path,
    path: &str,
    old_path: Option<&str>,
) -> Result<FileDiff, ApiError> {
    check_path(path)?;
    if let Some(old) = old_path {
        check_path(old)?;
    }
    if !is_repository(root).await {
        return Err(ApiError::new(
            ErrorCode::Unsupported,
            "the workspace is not a git repository",
        ));
    }
    let spec = literal(path);
    let untracked = git_ok(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            &spec,
        ],
        4096,
    )
    .await?;
    let output = if !untracked.stdout.is_empty() {
        let relative = format!("./{path}");
        let output = git(
            root,
            &[
                "diff",
                "--no-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "/dev/null",
                &relative,
            ],
            MAX_DIFF,
        )
        .await?;
        if output.status > 1 || output.status < 0 {
            return Err(internal(format!(
                "git diff exited with status {}",
                output.status
            )));
        }
        output
    } else {
        let base = base(root).await;
        let old_spec = old_path.map(literal);
        let mut args = vec![
            "diff",
            base,
            "--relative",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
            "--",
        ];
        if let Some(old) = &old_spec {
            args.push(old);
        }
        args.push(&spec);
        git_ok(root, &args, MAX_DIFF).await?
    };
    let (diff, truncated) = cut(output.stdout, output.truncated);
    Ok(FileDiff {
        path: path.to_string(),
        binary: is_binary_diff(&diff),
        diff,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_status_with_renames() {
        let data = b"M\0src/a.rs\0R087\0old.txt\0new.txt\0U\0conflict.txt\0A\0added.rs\0";
        let files = parse_name_status(data);
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].status, ChangeStatus::Modified);
        assert_eq!(files[1].status, ChangeStatus::Renamed);
        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].old_path.as_deref(), Some("old.txt"));
        assert_eq!(files[2].status, ChangeStatus::Conflicted);
        assert_eq!(files[3].status, ChangeStatus::Added);
    }

    #[test]
    fn parses_numstat_with_renames_and_binaries() {
        let data = b"3\t1\tsrc/a.rs\x002\t0\t\0old.txt\0new.txt\0-\t-\timage.png\0";
        let stats = parse_numstat(data);
        assert_eq!(stats["src/a.rs"], (3, 1, false));
        assert_eq!(stats["new.txt"], (2, 0, false));
        assert_eq!(stats["image.png"], (0, 0, true));
    }

    #[test]
    fn rejects_paths_outside_the_workspace() {
        assert!(check_path("src/a.rs").is_ok());
        assert!(check_path("../etc/passwd").is_err());
        assert!(check_path("/etc/passwd").is_err());
        assert!(check_path("a/../../b").is_err());
        assert!(check_path("").is_err());
    }

    #[test]
    fn truncates_diffs_at_a_line_boundary() {
        let mut data = b"line\n".repeat(MAX_DIFF / 5 + 10);
        data.extend_from_slice(b"partial");
        let (text, truncated) = cut(data, false);
        assert!(truncated);
        assert!(text.ends_with('\n'));
        assert!(text.len() <= MAX_DIFF);
    }
}
