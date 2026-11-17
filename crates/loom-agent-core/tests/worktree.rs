//! Sessions that work in a git worktree of their own.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{Harness, calls, outcome, run_turn, test_config, text};
use loom_agent_core::worktree::CreateOptions;
use loom_config::PolicyAction;
use loom_llm::{InputContent, Message};
use loom_proto::{ErrorCode, MergeStrategy, TurnOutcome};
use serde_json::json;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn repository(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    std::fs::write(dir.join("README"), "hello\n").unwrap();
    std::fs::write(dir.join(".gitignore"), ".env\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

fn commit_file(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", &format!("change {name}")]);
}

fn tool_results(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { content, .. } => content.iter().find_map(|part| match part {
                InputContent::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

fn harness(scripts: Vec<common::Script>) -> Harness {
    Harness::with_workspace(
        test_config(PolicyAction::Allow),
        scripts,
        repository,
        |_| {},
    )
}

#[tokio::test]
async fn a_session_works_in_its_own_worktree() {
    let h = Harness::with_workspace(
        test_config(PolicyAction::Allow),
        vec![
            calls(&[
                (
                    "c1",
                    "write",
                    json!({"path": "new.txt", "content": "new\n"}),
                ),
                (
                    "c2",
                    "shell",
                    json!({"command": "git add -A && git commit -q -m 'add new' && echo \"token=$TOKEN\""}),
                ),
            ]),
            text("done"),
        ],
        |ws| {
            repository(ws);
            std::fs::write(ws.join(".env"), "TOKEN=worktree-secret-1\n").unwrap();
        },
        |_| {},
    );
    let session = h
        .agent
        .create_worktree_session(None, CreateOptions::default())
        .await
        .unwrap();
    let info = session.info().worktree.unwrap();
    let short: String = session
        .id()
        .0
        .chars()
        .filter(|c| *c != '-')
        .take(8)
        .collect();
    assert_eq!(info.branch, format!("loom/{short}"));
    assert_eq!(info.base, "main");
    let path = PathBuf::from(&info.path);
    let dir = path.parent().unwrap();
    assert_eq!(dir.parent().unwrap(), h.dirs.config_dir().join("worktrees"));
    let mode = std::fs::metadata(dir).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert_eq!(session.workspace().root(), path.canonicalize().unwrap());

    let mut sub = session.subscribe(None);
    let events = run_turn(&session, &mut sub, "add a file").await;
    assert_eq!(outcome(&events), Some(TurnOutcome::Completed));
    assert!(path.join("new.txt").exists());
    assert!(!h.workspace.join("new.txt").exists());
    let results = tool_results(&session.history());
    assert!(results[1].contains("token=[REDACTED:TOKEN]"), "{results:?}");

    let status = h.agent.worktree_status(session.id()).await.unwrap();
    assert_eq!((status.ahead, status.behind), (1, 0));
    assert!(!status.dirty && !status.merged && !status.missing);
    let prompt = session.system_prompt().await;
    assert!(prompt.text.contains(&info.path), "{}", prompt.text);

    let id = session.id().clone();
    h.agent.shutdown().await;
    drop(session);
    let (agent, _) = h.reopen(test_config(PolicyAction::Allow), vec![]);
    let restored = agent.session(&id).unwrap();
    assert_eq!(restored.info().worktree.unwrap(), info);
    assert_eq!(restored.workspace().root(), path.canonicalize().unwrap());
}

#[tokio::test]
async fn worktrees_end_with_a_merge_or_a_squash_merge() {
    let h = harness(vec![]);
    let agent = &h.agent;
    let main = &h.workspace;
    let create = || agent.create_worktree_session(None, CreateOptions::default());

    let merged = create().await.unwrap();
    let path = PathBuf::from(merged.info().worktree.unwrap().path);
    commit_file(&path, "a.txt", "a\n");
    let err = agent.remove_worktree(merged.id(), false).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    std::fs::write(main.join("README"), "local\n").unwrap();
    let err = agent
        .merge_worktree(merged.id(), MergeStrategy::Merge, false)
        .await
        .unwrap_err();
    assert!(err.message.contains("uncommitted"), "{err:?}");
    git(main, &["checkout", "README"]);
    let commit = agent
        .merge_worktree(merged.id(), MergeStrategy::Merge, false)
        .await
        .unwrap();
    assert_eq!(git(main, &["rev-parse", "HEAD"]), commit);
    assert!(main.join("a.txt").exists());
    assert!(agent.worktree_status(merged.id()).await.unwrap().merged);
    agent.remove_worktree(merged.id(), false).await.unwrap();
    assert!(merged.info().worktree.is_none());
    assert!(!path.exists());
    assert_eq!(merged.workspace().root(), main.canonicalize().unwrap());

    let squashed = create().await.unwrap();
    let path = PathBuf::from(squashed.info().worktree.unwrap().path);
    commit_file(&path, "b.txt", "b\n");
    commit_file(&path, "c.txt", "c\n");
    let squash = MergeStrategy::Squash {
        message: "Add b and c".into(),
    };
    agent
        .merge_worktree(squashed.id(), squash, true)
        .await
        .unwrap();
    assert_eq!(git(main, &["log", "-1", "--format=%s"]), "Add b and c");
    assert_eq!(git(main, &["rev-list", "--count", "HEAD^@"]), "3");
    assert!(main.join("c.txt").exists() && !path.exists());
    assert!(squashed.info().worktree.is_none());

    let conflicted = create().await.unwrap();
    let path = PathBuf::from(conflicted.info().worktree.unwrap().path);
    commit_file(&path, "README", "theirs\n");
    commit_file(main, "README", "ours\n");
    let head = git(main, &["rev-parse", "HEAD"]);
    for strategy in [
        MergeStrategy::Merge,
        MergeStrategy::Squash {
            message: "x".into(),
        },
    ] {
        let err = agent
            .merge_worktree(conflicted.id(), strategy, true)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(err.message.contains("README"), "{err:?}");
        assert_eq!(git(main, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(main, &["status", "--porcelain"]), "");
        assert!(path.exists());
    }

    let err = agent
        .delete_session_and_worktree(conflicted.id())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    agent.remove_worktree(conflicted.id(), true).await.unwrap();
    agent
        .delete_session_and_worktree(conflicted.id())
        .await
        .unwrap();
    assert!(!path.exists());
    assert_eq!(git(main, &["branch", "--list", "loom/*"]), "");
}

#[tokio::test]
async fn pull_requests_push_the_branch_and_run_gh() {
    let tools = tempfile::tempdir().unwrap();
    let remote = tools.path().join("remote.git");
    let calls_file = tools.path().join("gh-calls");
    let gh = tools.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\necho 'Creating pull request'\necho https://github.com/o/r/pull/7\n",
            calls_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let gh_program = gh.display().to_string();
    let h = Harness::with_workspace(
        test_config(PolicyAction::Allow),
        vec![],
        |ws| {
            repository(ws);
            git(ws, &["init", "-q", "--bare", remote.to_str().unwrap()]);
            git(ws, &["remote", "add", "origin", remote.to_str().unwrap()]);
            git(ws, &["push", "-q", "-u", "origin", "main"]);
        },
        |options| options.gh = gh_program,
    );
    let session = h
        .agent
        .create_worktree_session(
            None,
            CreateOptions {
                branch: Some("feature/x".into()),
                base: None,
            },
        )
        .await
        .unwrap();
    let path = PathBuf::from(session.info().worktree.unwrap().path);
    let err = h
        .agent
        .open_pull_request(session.id(), "Add x", "", false)
        .await
        .unwrap_err();
    assert!(err.message.contains("no commits"), "{err:?}");
    commit_file(&path, "x.txt", "x\n");

    let url = h
        .agent
        .open_pull_request(session.id(), "Add x", "The body", true)
        .await
        .unwrap();
    assert_eq!(url, "https://github.com/o/r/pull/7");
    assert_eq!(
        session.info().worktree.unwrap().pull_request.as_deref(),
        Some(url.as_str())
    );
    assert_eq!(
        git(&remote, &["rev-parse", "feature/x"]),
        git(&path, &["rev-parse", "HEAD"])
    );
    let calls = std::fs::read_to_string(&calls_file).unwrap();
    assert_eq!(
        calls,
        "pr create --base main --head feature/x --title Add x --body The body --draft\n"
    );

    // A second call only pushes.
    commit_file(&path, "y.txt", "y\n");
    let again = h
        .agent
        .open_pull_request(session.id(), "", "", false)
        .await
        .unwrap();
    assert_eq!(again, url);
    assert_eq!(std::fs::read_to_string(&calls_file).unwrap(), calls);
    assert_eq!(
        git(&remote, &["rev-parse", "feature/x"]),
        git(&path, &["rev-parse", "HEAD"])
    );
}

#[tokio::test]
async fn a_workspace_outside_git_has_no_worktrees() {
    let h = Harness::new(test_config(PolicyAction::Allow), vec![]);
    let err = h
        .agent
        .create_worktree_session(None, CreateOptions::default())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::BadRequest);
    assert!(err.message.contains("not in a git repository"), "{err:?}");
    assert!(h.agent.list_sessions().is_empty());

    let h = harness(vec![]);
    for (branch, base, message) in [
        (Some("bad..name"), None, "not a valid branch name"),
        (None, Some("nope"), "not a branch or commit"),
        (Some("main"), None, "exists already"),
    ] {
        let err = h
            .agent
            .create_worktree_session(
                None,
                CreateOptions {
                    branch: branch.map(str::to_owned),
                    base: base.map(str::to_owned),
                },
            )
            .await
            .err()
            .unwrap();
        assert!(err.message.contains(message), "{err:?}");
    }
    assert!(h.agent.list_sessions().is_empty());
}
