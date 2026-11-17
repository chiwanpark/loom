//! The actions of sessions in git worktrees: create one, merge its branch,
//! open a pull request, and remove the worktree with its session.

use std::future::Future;

use gpui::{App, AsyncWindowContext, Context, Entity, Window};
use loom_client::proto::{MergeStrategy, SessionInfo, WorktreeOptions};

use crate::conversation::short_id;
use crate::state::AgentConnection;
use crate::ui::dialogs::{self, FormField};
use crate::ui::notify;
use crate::ui::workspace::Workspace;

impl Workspace {
    pub(crate) fn new_worktree_session(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        dialogs::open_form(
            "New session in a worktree",
            Some(
                "The session works in a new git worktree on a branch of its own, outside the workspace. When it is done, merge the branch or open a pull request from the session's menu."
                    .into(),
            ),
            vec![
                FormField::new("Branch", "loom/<session> (empty for the default)"),
                FormField::new("Base", "the branch of the workspace (empty for the default)"),
            ],
            "Create",
            move |values, window, cx| {
                let mut values = values
                    .into_iter()
                    .map(|v| Some(v.trim().to_owned()).filter(|v| !v.is_empty()));
                let options = WorktreeOptions {
                    branch: values.next().flatten(),
                    base: values.next().flatten(),
                };
                let conn = conn.clone();
                let task = conn.read(cx).create_worktree_session(options);
                let _ = this.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
                        let result = task.await;
                        let _ = this.update_in(cx, |this, window, cx| match result {
                            Some(Ok(info)) => {
                                conn.update(cx, |conn, cx| {
                                    conn.update_session_info(info.clone(), cx)
                                });
                                this.open_session(conn.clone(), info, window, cx);
                            }
                            Some(Err(err)) => notify::error(
                                window,
                                cx,
                                format!("Could not create the worktree: {err}"),
                            ),
                            None => {}
                        });
                    })
                    .detach();
                });
            },
            window,
            cx,
        );
    }

    /// Runs a worktree action, then reports its result or error.
    fn worktree_action<T: 'static>(
        &mut self,
        task: impl Future<Output = Option<loom_client::Result<T>>> + 'static,
        done: impl FnOnce(T, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Some(Ok(value)) => done(value, window, cx),
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
    }

    pub(crate) fn merge_worktree(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = info.worktree.clone() else {
            return;
        };
        let this = cx.entity().downgrade();
        let title = info.title.clone().unwrap_or_default();
        dialogs::open_form(
            "Merge worktree",
            Some(format!(
                "Merges {} into {} in the workspace, which must have {} checked out and no uncommitted changes. Enter a commit message to squash the branch into one commit, or make it empty for a merge commit that keeps the commits of the branch. The worktree and the session stay; to delete both, choose Remove worktree in the session's menu.",
                worktree.branch, worktree.base, worktree.base
            )),
            vec![FormField::new("Squash commit message", "empty for a merge commit").value(title)],
            "Merge",
            move |values, window, cx| {
                let message = values.into_iter().next().unwrap_or_default();
                let strategy = if message.trim().is_empty() {
                    MergeStrategy::Merge
                } else {
                    MergeStrategy::Squash { message }
                };
                let task = conn.read(cx).merge_worktree(info.id.clone(), strategy);
                let conn = conn.clone();
                let _ = this.update(cx, |ws, cx| {
                    ws.worktree_action(
                        task,
                        move |commit, window, cx| {
                            conn.update(cx, |conn, cx| conn.refresh_changes(cx));
                            notify::success(window, cx, format!("Merged as {}", short_id(&commit)));
                        },
                        window,
                        cx,
                    )
                });
            },
            window,
            cx,
        );
    }

    pub(crate) fn open_pull_request(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = info.worktree.clone() else {
            return;
        };
        let done = |url: String, window: &mut Window, cx: &mut App| {
            notify::success(window, cx, format!("Pull request: {url}"));
        };
        if worktree.pull_request.is_some() {
            let task =
                conn.read(cx)
                    .open_pull_request(info.id, String::new(), String::new(), false);
            self.worktree_action(task, done, window, cx);
            return;
        }
        let this = cx.entity().downgrade();
        dialogs::open_form(
            "Open pull request",
            Some(format!(
                "Pushes {} and opens a pull request into {} with the GitHub CLI (gh) on the agent's host. Type \"draft\" in the last field to open a draft.",
                worktree.branch, worktree.base
            )),
            vec![
                FormField::new("Title", "").value(info.title.clone().unwrap_or_default()),
                FormField::new("Description", ""),
                FormField::new("Draft", "empty, or draft"),
            ],
            "Open",
            move |values, window, cx| {
                let mut values = values.into_iter();
                let title = values.next().unwrap_or_default();
                let body = values.next().unwrap_or_default();
                let draft = values
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("draft"));
                let task = conn
                    .read(cx)
                    .open_pull_request(info.id.clone(), title, body, draft);
                let _ = this.update(cx, |ws, cx| ws.worktree_action(task, done, window, cx));
            },
            window,
            cx,
        );
    }

    pub(crate) fn remove_worktree(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = info.worktree.clone() else {
            return;
        };
        let status = conn.read(cx).worktree_status(info.id.clone());
        let this = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            let status = status.await.and_then(Result::ok);
            let _ = cx.update(|window, cx| {
                let unmerged = status.as_ref().is_some_and(|s| s.dirty || !s.merged);
                let warning = if unmerged {
                    " The worktree has uncommitted changes or commits that were not merged. You lose them."
                } else {
                    ""
                };
                dialogs::confirm(
                    "Remove the worktree?",
                    format!(
                        "Removes the worktree, the branch {}, and the session with its history. This cannot be undone.{warning}",
                        worktree.branch
                    ),
                    "Remove",
                    move |window, cx| {
                        let (conn, id) = (conn.clone(), info.id.clone());
                        let _ = this.update(cx, |ws, cx| {
                            ws.drop_session_guarded(conn, id, window, cx)
                        });
                    },
                    window,
                    cx,
                );
            });
        })
        .detach();
    }
}
