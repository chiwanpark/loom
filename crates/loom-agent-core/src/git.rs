use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use loom_proto::{
    ApiError, ChangeStatus, CommitInfo, CommittedChanges, ErrorCode, FileChange, FileDiff,
    WorkspaceChanges,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_LISTING: usize = 8 * 1024 * 1024;
const MAX_FILES: usize = 2000;
const MAX_COMMITS: usize = 200;
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
    git_with_input(root, args, cap, None).await
}

async fn git_with_input(
    root: &Path,
    args: &[&str],
    cap: usize,
    input: Option<Vec<u8>>,
) -> Result<Output, ApiError> {
    let mut child = Command::new("git")
        .args(PINNED_CONFIG)
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| internal(format!("cannot run git: {err}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
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

/// What the commits of the checked-out branch are compared with.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommitBase<'a> {
    /// The base branch, or `None` for the upstream of the checked-out
    /// branch.
    pub branch: Option<&'a str>,
    /// The commit of the branch at its last merge into the base, which a
    /// squash merge leaves out of the history of the base.
    pub merged_head: Option<&'a str>,
}

/// The files that differ between `from` and `to`, or the working tree when
/// `to` is `None`, with their line counts; and whether there were too many.
async fn diff_files(
    root: &Path,
    from: &str,
    to: Option<&str>,
) -> Result<(Vec<FileChange>, bool), ApiError> {
    let mut diff_args = vec![
        "diff",
        from,
        "--relative",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-M",
        "-z",
    ];
    if let Some(to) = to {
        diff_args.insert(2, to);
    }
    let mut args = diff_args.clone();
    args.push("--name-status");
    let names = git_ok(root, &args, MAX_LISTING).await?;
    let mut args = diff_args;
    args.push("--numstat");
    let stats = git_ok(root, &args, MAX_LISTING).await?;
    let truncated = names.truncated || stats.truncated;
    let stats = parse_numstat(&stats.stdout);
    let mut files = parse_name_status(&names.stdout);
    for file in &mut files {
        if let Some((added, removed, binary)) = stats.get(&file.path) {
            file.added = *added;
            file.removed = *removed;
            file.binary = *binary;
        }
    }
    Ok((files, truncated))
}

async fn is_ancestor(root: &Path, ancestor: &str, of: &str) -> bool {
    matches!(
        git(root, &["merge-base", "--is-ancestor", ancestor, of], 64).await,
        Ok(output) if output.status == 0
    )
}

/// The name of the base and the commit after which the branch's own
/// commits come, or `None` when the branch has no base.
async fn commit_base(root: &Path, base: CommitBase<'_>) -> Option<(String, String)> {
    let line = |output: Output| {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status == 0 && !text.is_empty()).then_some(text)
    };
    let name = match base.branch {
        Some(branch) if branch.starts_with('-') => return None,
        Some(branch) => branch.to_owned(),
        None => git(
            root,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}",
            ],
            1024,
        )
        .await
        .ok()
        .and_then(line)?,
    };
    let mut since = git(root, &["merge-base", &name, "HEAD"], 256)
        .await
        .ok()
        .and_then(line)?;
    if let Some(merged) = base.merged_head
        && merged != since
        && !merged.starts_with('-')
        && is_ancestor(root, &since, merged).await
        && is_ancestor(root, merged, "HEAD").await
    {
        since = merged.to_owned();
    }
    Some((name, since))
}

fn parse_log(data: &[u8]) -> Vec<CommitInfo> {
    data.split(|b| *b == 0x1e)
        .filter_map(|record| {
            let record = String::from_utf8_lossy(record);
            let record = record.trim_start_matches('\n');
            let mut fields = record.splitn(4, '\x1f');
            let id = fields.next().filter(|id| !id.is_empty())?;
            let author = fields.next()?;
            let time = fields.next()?.parse().unwrap_or(0);
            let summary = fields.next()?;
            Some(CommitInfo {
                id: id.to_owned(),
                summary: summary.to_owned(),
                author: author.to_owned(),
                time,
            })
        })
        .collect()
}

/// The commits of the checked-out branch that `base` does not have, and
/// the files they change.
async fn committed(
    root: &Path,
    base: CommitBase<'_>,
) -> Result<Option<CommittedChanges>, ApiError> {
    let Some((name, since)) = commit_base(root, base).await else {
        return Ok(None);
    };
    let range = format!("{since}..HEAD");
    let count = format!("--max-count={}", MAX_COMMITS + 1);
    let log = git_ok(
        root,
        &[
            "log",
            "--no-color",
            &count,
            "--format=%H%x1f%an%x1f%ct%x1f%s%x1e",
            &range,
            "--",
            ".",
        ],
        MAX_LISTING,
    )
    .await?;
    let mut commits = parse_log(&log.stdout);
    let mut truncated = log.truncated || commits.len() > MAX_COMMITS;
    commits.truncate(MAX_COMMITS);
    let (mut files, cut) = diff_files(root, &since, Some("HEAD")).await?;
    truncated |= cut || files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Some(CommittedChanges {
        base: name,
        since,
        commits,
        files,
        truncated,
    }))
}

pub async fn changes(root: &Path, base: CommitBase<'_>) -> Result<WorkspaceChanges, ApiError> {
    if !is_repository(root).await {
        return Ok(WorkspaceChanges::default());
    }
    let branch = branch(root).await;
    let (mut files, mut truncated) = diff_files(root, self::base(root).await, None).await?;
    let untracked = git_ok(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        MAX_LISTING,
    )
    .await?;
    truncated |= untracked.truncated;
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
        committed: committed(root, base).await.ok().flatten(),
    })
}

pub async fn nested_repositories(root: &Path) -> Vec<String> {
    if is_repository(root).await {
        return Vec::new();
    }
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return Vec::new();
    };
    let mut found = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let dir = entry.path();
        if !name.starts_with('.')
            && tokio::fs::metadata(&dir).await.is_ok_and(|m| m.is_dir())
            && tokio::fs::symlink_metadata(dir.join(".git")).await.is_ok()
        {
            found.push(name);
        }
    }
    found.sort();
    found
}

fn prefixed(directory: &str, mut file: FileChange) -> FileChange {
    file.path = format!("{directory}/{}", file.path);
    file.old_path = file.old_path.map(|old| format!("{directory}/{old}"));
    file
}

pub async fn nested_changes(
    root: &Path,
    directories: &[String],
) -> Result<WorkspaceChanges, ApiError> {
    let mut all = WorkspaceChanges {
        repository: !directories.is_empty(),
        ..WorkspaceChanges::default()
    };
    let mut branches = Vec::new();
    let mut committed: Option<CommittedChanges> = None;
    for directory in directories {
        let changes = changes(&root.join(directory), CommitBase::default()).await?;
        if !changes.repository {
            continue;
        }
        branches.push(changes.branch.clone());
        all.truncated |= changes.truncated;
        all.files.extend(
            changes
                .files
                .into_iter()
                .map(|file| prefixed(directory, file)),
        );
        if let Some(commits) = changes.committed {
            let merged = committed.get_or_insert_with(|| CommittedChanges {
                base: commits.base.clone(),
                since: commits.since.clone(),
                commits: Vec::new(),
                files: Vec::new(),
                truncated: false,
            });
            if merged.base != commits.base {
                merged.base = "the upstream branches".to_owned();
            }
            merged.truncated |= commits.truncated;
            merged.commits.extend(commits.commits);
            merged.files.extend(
                commits
                    .files
                    .into_iter()
                    .map(|file| prefixed(directory, file)),
            );
        }
    }
    branches.dedup();
    if let [branch] = branches.as_slice() {
        all.branch = branch.clone();
    }
    if let Some(committed) = &mut committed {
        committed.commits.sort_by_key(|c| std::cmp::Reverse(c.time));
    }
    all.committed = committed;
    Ok(all)
}

pub fn split_nested<'a>(
    directories: &[String],
    path: &'a str,
    old_path: Option<&'a str>,
) -> Option<(String, &'a str, Option<&'a str>)> {
    let (directory, rest) = path.split_once('/')?;
    if !directories.iter().any(|d| d == directory) {
        return None;
    }
    let old = match old_path {
        Some(old) => Some(
            old.strip_prefix(directory)
                .and_then(|o| o.strip_prefix('/'))?,
        ),
        None => None,
    };
    Some((directory.to_owned(), rest, old))
}

pub async fn ignored(root: &Path, paths: &[String]) -> HashSet<String> {
    if paths.is_empty() || !is_repository(root).await {
        return HashSet::new();
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    match git_with_input(
        root,
        &["check-ignore", "-z", "--stdin"],
        MAX_LISTING,
        Some(input),
    )
    .await
    {
        Ok(output) if output.status == 0 || output.status == 1 => fields(&output.stdout).collect(),
        _ => HashSet::new(),
    }
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

/// The diff of a file: its uncommitted changes against `HEAD`, or with
/// `committed`, its changes in the commits since that base.
pub async fn file_diff(
    root: &Path,
    path: &str,
    old_path: Option<&str>,
    committed: Option<CommitBase<'_>>,
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
    let old_spec = old_path.map(literal);
    let since = match committed {
        Some(base) => Some(commit_base(root, base).await.ok_or_else(|| {
            ApiError::new(
                ErrorCode::NotFound,
                "the branch has no base to compare with",
            )
        })?),
        None => None,
    };
    let untracked = match since {
        Some(_) => false,
        None => !git_ok(
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
        .await?
        .stdout
        .is_empty(),
    };
    let output = if !untracked {
        let mut args = match &since {
            Some((_, since)) => vec!["diff", since.as_str(), "HEAD"],
            None => vec!["diff", base(root).await],
        };
        args.extend([
            "--relative",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
            "--",
        ]);
        args.extend(old_spec.as_deref());
        args.push(&spec);
        git_ok(root, &args, MAX_DIFF).await?
    } else {
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
    fn parses_the_log() {
        let data = b"aaa\x1fAda\x1f1700000000\x1fFix a\x1fb\x1e\nbbb\x1fBo\x1fx\x1f\x1e\n";
        let commits = parse_log(data);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].id, "aaa");
        assert_eq!(commits[0].author, "Ada");
        assert_eq!(commits[0].time, 1_700_000_000);
        assert_eq!(commits[0].summary, "Fix a\x1fb");
        assert_eq!((commits[1].time, commits[1].summary.as_str()), (0, ""));
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
