//! Git worktrees of sessions: a session can work on a branch of its own in a
//! worktree outside the workspace, and end with a merge, a squash merge, or a
//! pull request.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use loom_proto::{ApiError, ErrorCode, SessionId, WorktreeInfo, WorktreeStatus};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// Pushes and `gh` reach the network.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(180);
/// The most of a command's error output that an error message shows.
const MAX_STDERR: usize = 2000;

/// The worktree of a session, as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// The top directory of the worktree.
    pub path: PathBuf,
    /// The session's root in the worktree: `path`, or the directory in it
    /// that matches the workspace when the workspace is not the top of its
    /// repository.
    pub root: PathBuf,
    pub branch: String,
    /// The branch that the worktree started from, and that merges go into.
    pub base: String,
    #[serde(default)]
    pub pull_request: Option<String>,
    /// The commit of the branch at its last merge.
    #[serde(default)]
    pub merged_head: Option<String>,
    /// The directory of the repository that the worktrees share, which
    /// commands in the sandbox must be able to write when they commit.
    #[serde(default)]
    pub git_dir: Option<PathBuf>,
}

impl Worktree {
    pub fn info(&self) -> WorktreeInfo {
        WorktreeInfo {
            branch: self.branch.clone(),
            base: self.base.clone(),
            path: self.root.display().to_string(),
            pull_request: self.pull_request.clone(),
        }
    }
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Conflict, message)
}

fn bad(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::BadRequest, message)
}

fn internal(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Internal, message)
}

async fn run(
    program: &str,
    dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, ApiError> {
    let child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                ApiError::new(
                    ErrorCode::Unsupported,
                    format!("{program} is not installed on the agent's host"),
                )
            } else {
                internal(format!("cannot run {program}: {err}"))
            }
        })?;
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| internal(format!("{program} {} timed out", args[0])))?
        .map_err(|err| internal(format!("{program} failed: {err}")))?;
    Ok(Output {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

async fn git(dir: &Path, args: &[&str]) -> Result<Output, ApiError> {
    run("git", dir, args, GIT_TIMEOUT).await
}

/// The output of a git command that must succeed.
async fn git_ok(dir: &Path, args: &[&str]) -> Result<String, ApiError> {
    let output = git(dir, args).await?;
    if !output.ok {
        return Err(failed("git", args, &output));
    }
    Ok(output.stdout)
}

fn failed(program: &str, args: &[&str], output: &Output) -> ApiError {
    let mut stderr = output.stderr.clone();
    if stderr.len() > MAX_STDERR {
        let mut cut = MAX_STDERR;
        while !stderr.is_char_boundary(cut) {
            cut -= 1;
        }
        stderr.truncate(cut);
        stderr.push('…');
    }
    internal(format!("{program} {} failed: {stderr}", args[0]))
}

/// The directory of the worktrees of the workspace at `workspace`.
pub fn workspace_dir(worktrees: &Path, workspace: &Path) -> PathBuf {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let key: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    worktrees.join(key)
}

/// The branch and base that a new worktree gets.
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    pub branch: Option<String>,
    pub base: Option<String>,
}

/// Adds a worktree for `session` of the workspace at `workspace`, in
/// `worktrees`, on a new branch.
pub async fn create(
    workspace: &Path,
    worktrees: &Path,
    session: &SessionId,
    options: CreateOptions,
) -> Result<Worktree, ApiError> {
    let top = git(workspace, &["rev-parse", "--show-toplevel"]).await?;
    if !top.ok {
        return Err(bad(
            "the workspace is not in a git repository, so a session cannot have a worktree",
        ));
    }
    let prefix = git_ok(workspace, &["rev-parse", "--show-prefix"]).await?;
    let base = match options.base.filter(|b| !b.trim().is_empty()) {
        Some(base) => base.trim().to_owned(),
        None => {
            let head = git(workspace, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
            if !head.ok || head.stdout.is_empty() {
                return Err(bad(
                    "the workspace has no branch checked out; choose the base branch of the worktree",
                ));
            }
            head.stdout
        }
    };
    let verify = format!("{base}^{{commit}}");
    if !git(workspace, &["rev-parse", "--verify", "-q", &verify])
        .await?
        .ok
    {
        return Err(bad(format!("the base {base:?} is not a branch or commit")));
    }
    let short: String = session.0.chars().filter(|c| *c != '-').take(8).collect();
    let branch = match options.branch.filter(|b| !b.trim().is_empty()) {
        Some(branch) => branch.trim().to_owned(),
        None => format!("loom/{short}"),
    };
    if !git(workspace, &["check-ref-format", "--branch", &branch])
        .await?
        .ok
    {
        return Err(bad(format!("{branch:?} is not a valid branch name")));
    }
    let reference = format!("refs/heads/{branch}");
    if git(workspace, &["show-ref", "--verify", "-q", &reference])
        .await?
        .ok
    {
        return Err(conflict(format!("the branch {branch} exists already")));
    }
    let dir = workspace_dir(worktrees, workspace);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|err| internal(format!("cannot create {}: {err}", dir.display())))?;
    let path = dir.join(&short);
    if path.exists() {
        return Err(conflict(format!("{} exists already", path.display())));
    }
    let path_text = path.to_string_lossy().into_owned();
    git_ok(
        workspace,
        &["worktree", "add", "-q", "-b", &branch, &path_text, &base],
    )
    .await?;
    let root = match prefix.trim_end_matches('/') {
        "" => path.clone(),
        prefix => path.join(prefix),
    };
    Ok(Worktree {
        path,
        root,
        branch,
        base,
        pull_request: None,
        merged_head: None,
        git_dir: common_dir(workspace).await,
    })
}

/// The directory of the repository of `workspace` that its worktrees share,
/// which commands in a worktree write to when they commit.
pub async fn common_dir(workspace: &Path) -> Option<PathBuf> {
    let output = git(
        workspace,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .ok()?;
    output.ok.then(|| PathBuf::from(output.stdout))
}

async fn head(dir: &Path, rev: &str) -> Result<String, ApiError> {
    git_ok(dir, &["rev-parse", "--verify", "-q", rev]).await
}

/// The commit that `branch` points to.
pub async fn branch_head(workspace: &Path, branch: &str) -> Option<String> {
    head(workspace, &format!("refs/heads/{branch}")).await.ok()
}

async fn is_dirty(dir: &Path) -> Result<bool, ApiError> {
    Ok(!git_ok(dir, &["status", "--porcelain"]).await?.is_empty())
}

/// Where the worktree's branch is against its base.
pub async fn status(workspace: &Path, worktree: &Worktree) -> Result<WorktreeStatus, ApiError> {
    if !worktree.path.is_dir() {
        return Ok(WorktreeStatus {
            missing: true,
            ..WorktreeStatus::default()
        });
    }
    let range = format!("{}...{}", worktree.base, worktree.branch);
    let counts = git_ok(workspace, &["rev-list", "--left-right", "--count", &range]).await?;
    let mut numbers = counts
        .split_whitespace()
        .map(|n| n.parse::<u32>().unwrap_or(0));
    let behind = numbers.next().unwrap_or(0);
    let ahead = numbers.next().unwrap_or(0);
    let branch_head = head(workspace, &worktree.branch).await.ok();
    Ok(WorktreeStatus {
        ahead,
        behind,
        dirty: is_dirty(&worktree.path).await?,
        missing: false,
        merged: ahead == 0 || (branch_head.is_some() && branch_head == worktree.merged_head),
    })
}

/// Merges the worktree's branch into its base in the workspace, which must
/// have the base checked out and no changes to tracked files. With a
/// message, squashes the branch into one commit. Returns the new commit.
pub async fn merge(
    workspace: &Path,
    worktree: &Worktree,
    squash: Option<&str>,
) -> Result<String, ApiError> {
    let current = git(workspace, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
    if current.stdout != worktree.base {
        return Err(conflict(format!(
            "the workspace must have {} checked out to merge into it",
            worktree.base
        )));
    }
    if !git_ok(
        workspace,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .await?
    .is_empty()
    {
        return Err(conflict(
            "the workspace has uncommitted changes; commit or stash them before the merge",
        ));
    }
    let status = status(workspace, worktree).await?;
    if status.missing {
        return Err(conflict("the worktree is missing"));
    }
    if status.dirty {
        return Err(conflict(
            "the worktree has uncommitted changes; commit or discard them before the merge",
        ));
    }
    if status.ahead == 0 {
        return Err(conflict(format!(
            "{} has no commits that {} does not have",
            worktree.branch, worktree.base
        )));
    }
    let message = format!("Merge branch '{}'", worktree.branch);
    let args: Vec<&str> = match squash {
        Some(_) => vec!["merge", "--squash", &worktree.branch],
        None => vec!["merge", "--no-ff", "-m", &message, &worktree.branch],
    };
    let merged = git(workspace, &args).await?;
    if !merged.ok {
        let files = git(workspace, &["diff", "--name-only", "--diff-filter=U"])
            .await
            .map(|o| o.stdout)
            .unwrap_or_default();
        let undo: &[&str] = if squash.is_some() {
            &["reset", "--merge"]
        } else {
            &["merge", "--abort"]
        };
        let _ = git(workspace, undo).await;
        if files.is_empty() {
            return Err(failed("git", &args, &merged));
        }
        return Err(conflict(format!(
            "the merge has conflicts, so the workspace stays as it was. Resolve them in the worktree, for example by merging {} into {} there, then merge again. Files with conflicts: {}",
            worktree.base,
            worktree.branch,
            files.lines().collect::<Vec<_>>().join(", ")
        )));
    }
    if let Some(message) = squash {
        let message = if message.trim().is_empty() {
            format!("Squashed branch '{}'", worktree.branch)
        } else {
            message.to_owned()
        };
        let commit = git(workspace, &["commit", "-q", "-m", &message]).await?;
        if !commit.ok {
            let _ = git(workspace, &["reset", "--merge"]).await;
            return Err(failed("git", &["commit"], &commit));
        }
    }
    head(workspace, "HEAD").await
}

/// Removes the worktree and its branch. Without `force`, refuses a worktree
/// with uncommitted changes or commits that were not merged.
pub async fn remove(workspace: &Path, worktree: &Worktree, force: bool) -> Result<(), ApiError> {
    let status = status(workspace, worktree).await?;
    if !force && !status.missing && (status.dirty || !status.merged) {
        return Err(conflict(format!(
            "the worktree of {} has {}; remove it anyway to lose them",
            worktree.branch,
            if status.dirty {
                "uncommitted changes"
            } else {
                "commits that were not merged"
            }
        )));
    }
    if status.missing {
        git_ok(workspace, &["worktree", "prune"]).await?;
    } else {
        let path = worktree.path.to_string_lossy().into_owned();
        git_ok(workspace, &["worktree", "remove", "--force", &path]).await?;
    }
    let reference = format!("refs/heads/{}", worktree.branch);
    if git(workspace, &["show-ref", "--verify", "-q", &reference])
        .await?
        .ok
    {
        git_ok(workspace, &["branch", "-D", &worktree.branch]).await?;
    }
    Ok(())
}

/// Pushes the worktree's branch and opens a pull request into its base with
/// `gh`, the GitHub CLI program. Returns the URL of the pull request. A
/// worktree that has one already only pushes.
pub async fn open_pull_request(
    gh: &str,
    workspace: &Path,
    worktree: &Worktree,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String, ApiError> {
    let status = status(workspace, worktree).await?;
    if status.missing {
        return Err(conflict("the worktree is missing"));
    }
    if status.dirty {
        return Err(conflict(
            "the worktree has uncommitted changes; commit or discard them first",
        ));
    }
    if status.ahead == 0 {
        return Err(conflict(format!(
            "{} has no commits that {} does not have",
            worktree.branch, worktree.base
        )));
    }
    if worktree.pull_request.is_none() && title.trim().is_empty() {
        return Err(bad("the pull request needs a title"));
    }
    let key = format!("branch.{}.remote", worktree.base);
    let remote = match git(workspace, &["config", "--get", &key]).await? {
        Output {
            ok: true, stdout, ..
        } if !stdout.is_empty() => stdout,
        _ => "origin".to_owned(),
    };
    let remotes = git_ok(workspace, &["remote"]).await?;
    if !remotes.lines().any(|r| r == remote) {
        return Err(conflict(format!(
            "the repository has no remote {remote:?} to push {} to",
            worktree.branch
        )));
    }
    let push_args = ["push", "-u", &remote, &worktree.branch];
    let pushed = run("git", &worktree.path, &push_args, NETWORK_TIMEOUT).await?;
    if !pushed.ok {
        return Err(failed("git", &push_args, &pushed));
    }
    if let Some(url) = &worktree.pull_request {
        return Ok(url.clone());
    }
    let mut args = vec![
        "pr",
        "create",
        "--base",
        &worktree.base,
        "--head",
        &worktree.branch,
        "--title",
        title,
        "--body",
        body,
    ];
    if draft {
        args.push("--draft");
    }
    let created = run(gh, &worktree.path, &args, NETWORK_TIMEOUT).await?;
    if !created.ok {
        return Err(failed(gh, &args, &created));
    }
    created
        .stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("http"))
        .map(str::to_owned)
        .ok_or_else(|| internal("gh did not print the URL of the pull request"))
}
