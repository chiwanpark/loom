use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use loom_agent_core::Repositories;
use loom_client::OracleClient;
use loom_client::oracle_proto::GitSetup;
use loom_github::{GitHub, Http, NewPullRequest, PullRequest, repository_of};
use loom_proto::{ApiError, ErrorCode, PullRequestResult, PushRepositories, RepositoryStatus};
use tokio::sync::{Mutex, watch};

use crate::codespace_git::git_output_within;

const STATUS_TIMEOUT: Duration = Duration::from_secs(60);
const PUSH_TIMEOUT: Duration = Duration::from_secs(600);
const PULL_REQUESTS_FILE: &str = "pull-requests.json";
const RELATED: &str = "\n\n---\nRelated pull requests from the same codespace:\n";
const MAX_TITLE: usize = 256;
const MAX_BODY: usize = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryClone {
    pub directory: String,
    pub path: PathBuf,
    pub url: String,
    pub provider: Option<String>,
    pub base_branch: Option<String>,
}

pub struct CodespaceRepositories {
    clones: Vec<RepositoryClone>,
    setup: watch::Receiver<GitSetup>,
    client: watch::Receiver<OracleClient>,
    http: Http,
    state: PathBuf,
    lock: Mutex<()>,
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::BadRequest, message)
}

async fn git(dir: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
    let output = git_output_within(args, Some(dir), timeout).await?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

async fn has_ref(dir: &Path, reference: &str) -> bool {
    git_output_within(
        &["rev-parse", "--verify", "--quiet", reference],
        Some(dir),
        STATUS_TIMEOUT,
    )
    .await
    .is_ok_and(|output| output.status.success())
}

async fn count(dir: &Path, args: &[&str]) -> anyhow::Result<u32> {
    let text = git(dir, args, STATUS_TIMEOUT).await?;
    text.parse()
        .with_context(|| format!("git {} did not give a number", args.join(" ")))
}

pub fn strip_related(body: &str) -> &str {
    if let Some(index) = body.find(RELATED) {
        &body[..index]
    } else if body.starts_with(RELATED.trim_start()) {
        ""
    } else {
        body
    }
}

pub fn with_related(body: &str, others: &[String]) -> String {
    let own = strip_related(body);
    let mut text = if own.is_empty() {
        RELATED.trim_start().to_owned()
    } else {
        format!("{own}{RELATED}")
    };
    for other in others {
        text.push_str(&format!("- {other}\n"));
    }
    text
}

struct Opened {
    index: usize,
    github: GitHub,
    owner: String,
    name: String,
    pull_request: PullRequest,
}

enum Outcome {
    Skipped(String),
    Opened(Box<Opened>, bool),
}

impl CodespaceRepositories {
    pub fn new(
        clones: Vec<RepositoryClone>,
        setup: watch::Receiver<GitSetup>,
        client: watch::Receiver<OracleClient>,
        state_dir: &Path,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            clones,
            setup,
            client,
            http: Http::new()?,
            state: state_dir.join(PULL_REQUESTS_FILE),
            lock: Mutex::new(()),
        })
    }

    fn pull_requests(&self) -> BTreeMap<String, String> {
        std::fs::read(&self.state)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save_pull_requests(&self, urls: &BTreeMap<String, String>) {
        let result = serde_json::to_vec(urls)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                let partial = self.state.with_extension("partial");
                std::fs::write(&partial, bytes)?;
                std::fs::rename(&partial, &self.state)?;
                Ok(())
            });
        if let Err(err) = result {
            tracing::warn!(error = %err, "cannot keep the pull requests");
        }
    }

    async fn base(&self, clone: &RepositoryClone) -> Option<String> {
        if let Some(base) = &clone.base_branch {
            return Some(base.clone());
        }
        let head = git(
            &clone.path,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            STATUS_TIMEOUT,
        )
        .await
        .ok()?;
        head.strip_prefix("origin/").map(str::to_owned)
    }

    async fn fill(
        &self,
        clone: &RepositoryClone,
        status: &mut RepositoryStatus,
    ) -> anyhow::Result<()> {
        let path = &clone.path;
        if !path.join(".git").exists() {
            bail!("the clone is missing");
        }
        status.branch = git(path, &["rev-parse", "--abbrev-ref", "HEAD"], STATUS_TIMEOUT).await?;
        let changes = git(
            path,
            &["status", "--porcelain=v1", "--untracked-files=all"],
            STATUS_TIMEOUT,
        )
        .await?;
        status.changed_files =
            u32::try_from(changes.lines().filter(|l| !l.is_empty()).count()).unwrap_or(u32::MAX);
        status.base_branch = self.base(clone).await;
        if let Some(base) = &status.base_branch {
            let reference = format!("refs/remotes/origin/{base}");
            if has_ref(path, &reference).await {
                status.ahead = count(
                    path,
                    &["rev-list", "--count", &format!("{reference}..HEAD")],
                )
                .await?;
            }
        }
        status.unpushed = count(
            path,
            &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"],
        )
        .await?;
        status.pushed = status.branch != "HEAD"
            && has_ref(path, &format!("refs/remotes/origin/{}", status.branch)).await;
        Ok(())
    }

    async fn status(
        &self,
        clone: &RepositoryClone,
        urls: &BTreeMap<String, String>,
    ) -> RepositoryStatus {
        let mut status = RepositoryStatus {
            directory: clone.directory.clone(),
            path: clone.path.to_string_lossy().into_owned(),
            url: clone.url.clone(),
            provider: clone.provider.clone(),
            branch: String::new(),
            base_branch: None,
            changed_files: 0,
            ahead: 0,
            unpushed: 0,
            pushed: false,
            pull_request: urls.get(&clone.directory).cloned(),
            error: None,
        };
        if let Err(err) = self.fill(clone, &mut status).await {
            status.error = Some(format!("{err:#}"));
        }
        status
    }

    async fn push_clone(
        &self,
        clone: &RepositoryClone,
        message: Option<&str>,
    ) -> anyhow::Result<()> {
        let path = &clone.path;
        if !path.join(".git").exists() {
            bail!("the clone is missing");
        }
        if let Some(message) = message {
            let changes = git(
                path,
                &["status", "--porcelain=v1", "--untracked-files=all"],
                STATUS_TIMEOUT,
            )
            .await?;
            if !changes.is_empty() {
                git(path, &["add", "--all"], STATUS_TIMEOUT).await?;
                git(
                    path,
                    &["commit", "--quiet", "--message", message],
                    STATUS_TIMEOUT,
                )
                .await?;
            }
        }
        let branch = git(path, &["rev-parse", "--abbrev-ref", "HEAD"], STATUS_TIMEOUT).await?;
        if branch == "HEAD" {
            bail!("the clone is not on a branch");
        }
        let unpushed = count(
            path,
            &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"],
        )
        .await?;
        if unpushed > 0 {
            git(
                path,
                &["push", "--quiet", "--set-upstream", "origin", &branch],
                PUSH_TIMEOUT,
            )
            .await?;
        }
        Ok(())
    }

    fn selected(&self, directories: &[String]) -> Result<Vec<&RepositoryClone>, ApiError> {
        for directory in directories {
            if !self.clones.iter().any(|c| &c.directory == directory) {
                return Err(ApiError::new(
                    ErrorCode::NotFound,
                    format!("the codespace has no repository in the directory {directory}"),
                ));
            }
        }
        Ok(self
            .clones
            .iter()
            .filter(|c| directories.is_empty() || directories.contains(&c.directory))
            .collect())
    }

    async fn open_one(
        &self,
        index: usize,
        clone: &RepositoryClone,
        setup: &GitSetup,
        client: &OracleClient,
        new: &NewPullRequest,
    ) -> anyhow::Result<Outcome> {
        let Some(provider_name) = &clone.provider else {
            return Ok(Outcome::Skipped(
                "the repository has no Git provider".to_owned(),
            ));
        };
        let provider = setup
            .providers
            .iter()
            .find(|p| &p.name == provider_name)
            .with_context(|| {
                format!("the codespace has no setup for the provider {provider_name}")
            })?;
        let (owner, name) = repository_of(&provider.web_url, &clone.url)
            .with_context(|| format!("{} is not a repository of {provider_name}", clone.url))?;
        let mut status = self.status(clone, &BTreeMap::new()).await;
        if let Some(error) = status.error.take() {
            bail!(error);
        }
        let base = status
            .base_branch
            .clone()
            .context("cannot find the base branch")?;
        if status.ahead == 0 {
            return Ok(Outcome::Skipped(format!("no commits ahead of {base}")));
        }
        if status.branch == "HEAD" {
            bail!("the clone is not on a branch");
        }
        if status.unpushed > 0 || !status.pushed {
            git(
                &clone.path,
                &[
                    "push",
                    "--quiet",
                    "--set-upstream",
                    "origin",
                    &status.branch,
                ],
                PUSH_TIMEOUT,
            )
            .await?;
        }
        let token = client
            .git_credential(&clone.url)
            .await
            .context("cannot get a token from the oracle")?
            .token
            .context("the preset has no Git credential for this repository")?;
        let github = GitHub::direct(
            self.http.clone(),
            &provider.api_url,
            Some(token.password.expose().to_owned()),
        );
        let head = format!("{owner}:{}", status.branch);
        let (pull_request, created) = match github.find_pull_request(&owner, &name, &head).await? {
            Some(found) => (found, false),
            None => {
                let new = NewPullRequest {
                    head: status.branch.clone(),
                    base,
                    ..new.clone()
                };
                (github.create_pull_request(&owner, &name, &new).await?, true)
            }
        };
        Ok(Outcome::Opened(
            Box::new(Opened {
                index,
                github,
                owner,
                name,
                pull_request,
            }),
            created,
        ))
    }
}

#[async_trait]
impl Repositories for CodespaceRepositories {
    async fn list(&self) -> Result<Vec<RepositoryStatus>, ApiError> {
        let urls = self.pull_requests();
        let mut statuses = Vec::new();
        for clone in &self.clones {
            statuses.push(self.status(clone, &urls).await);
        }
        Ok(statuses)
    }

    async fn push(&self, request: PushRepositories) -> Result<Vec<RepositoryStatus>, ApiError> {
        let message = request.commit_message.as_deref().map(str::trim);
        if message.is_some_and(str::is_empty) {
            return Err(bad_request("the commit message is empty"));
        }
        let selected = self.selected(&request.directories)?;
        let _guard = self.lock.lock().await;
        let urls = self.pull_requests();
        let mut statuses = Vec::new();
        for clone in selected {
            let pushed = self.push_clone(clone, message).await;
            let mut status = self.status(clone, &urls).await;
            if let Err(err) = pushed {
                status.error = Some(format!("{err:#}"));
            }
            statuses.push(status);
        }
        Ok(statuses)
    }

    async fn open_pull_requests(
        &self,
        title: String,
        body: String,
        draft: bool,
    ) -> Result<Vec<PullRequestResult>, ApiError> {
        let title = title.trim().to_owned();
        if title.is_empty() || title.len() > MAX_TITLE {
            return Err(bad_request(format!(
                "a pull request has a title of 1 to {MAX_TITLE} bytes"
            )));
        }
        if body.len() > MAX_BODY {
            return Err(bad_request(format!(
                "the body of a pull request has up to {MAX_BODY} bytes"
            )));
        }
        let _guard = self.lock.lock().await;
        let setup = self.setup.borrow().clone();
        let client = self.client.borrow().clone();
        let new = NewPullRequest {
            title,
            body,
            head: String::new(),
            base: String::new(),
            draft,
        };
        let mut results = Vec::new();
        let mut opened = Vec::new();
        for (index, clone) in self.clones.iter().enumerate() {
            let mut result = PullRequestResult {
                directory: clone.directory.clone(),
                url: None,
                created: false,
                skipped: None,
                error: None,
            };
            match self.open_one(index, clone, &setup, &client, &new).await {
                Ok(Outcome::Skipped(reason)) => result.skipped = Some(reason),
                Ok(Outcome::Opened(one, created)) => {
                    result.url = Some(one.pull_request.url.clone());
                    result.created = created;
                    opened.push(*one);
                }
                Err(err) => result.error = Some(format!("{err:#}")),
            }
            results.push(result);
        }
        if opened.len() > 1 {
            let links: Vec<String> = opened
                .iter()
                .map(|o| {
                    format!(
                        "{}/{}#{}: {}",
                        o.owner, o.name, o.pull_request.number, o.pull_request.url
                    )
                })
                .collect();
            for (position, one) in opened.iter().enumerate() {
                let others: Vec<String> = links
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != position)
                    .map(|(_, link)| link.clone())
                    .collect();
                let body = with_related(&one.pull_request.body, &others);
                if body == one.pull_request.body {
                    continue;
                }
                if let Err(err) = one
                    .github
                    .update_pull_request(&one.owner, &one.name, one.pull_request.number, &body)
                    .await
                {
                    results[one.index].error =
                        Some(format!("cannot link the other pull requests: {err}"));
                }
            }
        }
        let mut urls = self.pull_requests();
        for result in &results {
            if let Some(url) = &result.url {
                urls.insert(result.directory.clone(), url.clone());
            }
        }
        self.save_pull_requests(&urls);
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_links_replace_the_old_ones() {
        let others = vec!["acme/lib#2: https://x/acme/lib/pull/2".to_owned()];
        let body = with_related("Fixes it.", &others);
        assert_eq!(
            body,
            "Fixes it.\n\n---\nRelated pull requests from the same codespace:\n- acme/lib#2: https://x/acme/lib/pull/2\n"
        );
        assert_eq!(strip_related(&body), "Fixes it.");
        assert_eq!(with_related(&body, &others), body);
        let empty = with_related("", &others);
        assert!(empty.starts_with("---\nRelated"));
        assert_eq!(strip_related(&empty), "");
        assert_eq!(with_related(&empty, &others), empty);
    }
}
