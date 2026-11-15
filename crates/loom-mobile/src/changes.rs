//! What changed in an agent's workspace: its uncommitted changes in git,
//! the files a session changed, and their diffs, highlighted.

use std::path::Path;
use std::sync::Arc;

use loom_client::proto::{self, SessionId};
use loom_view_model::changes::{
    self, DiffLine, DiffLineKind, change_lines, diff_lines, display_path, session_changes,
};

use crate::agent::{AgentConnection, with_client};
use crate::error::{LoomError, Result};
use crate::highlight::{Span, highlight, syntax_for};
use crate::runtime::run;

/// How git sees a changed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    /// It became another kind of file, as a symbolic link.
    TypeChanged,
    /// New, and git does not track it yet.
    Untracked,
    /// A merge left conflicts in it.
    Conflicted,
}

impl From<proto::ChangeStatus> for ChangeKind {
    fn from(status: proto::ChangeStatus) -> Self {
        match status {
            proto::ChangeStatus::Modified => Self::Modified,
            proto::ChangeStatus::Added => Self::Added,
            proto::ChangeStatus::Deleted => Self::Deleted,
            proto::ChangeStatus::Renamed => Self::Renamed,
            proto::ChangeStatus::Copied => Self::Copied,
            proto::ChangeStatus::TypeChanged => Self::TypeChanged,
            proto::ChangeStatus::Untracked => Self::Untracked,
            proto::ChangeStatus::Conflicted => Self::Conflicted,
        }
    }
}

/// A file with uncommitted changes.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GitChange {
    /// Its path in the workspace.
    pub path: String,
    /// Where a renamed or copied file came from.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
}

/// The uncommitted changes of the workspace.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GitChanges {
    /// Whether the workspace is a git repository; without one there are no
    /// changes to list.
    pub repository: bool,
    pub branch: Option<String>,
    pub files: Vec<GitChange>,
    /// Whether there are more changed files than the agent lists.
    pub truncated: bool,
}

impl From<proto::WorkspaceChanges> for GitChanges {
    fn from(changes: proto::WorkspaceChanges) -> Self {
        Self {
            repository: changes.repository,
            branch: changes.branch,
            files: changes
                .files
                .into_iter()
                .map(|file| GitChange {
                    path: file.path,
                    old_path: file.old_path,
                    kind: file.status.into(),
                    added: file.added,
                    removed: file.removed,
                    binary: file.binary,
                })
                .collect(),
            truncated: changes.truncated,
        }
    }
}

/// A file a session changed with its tool calls.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SessionChange {
    /// Its path as the tool calls named it, for its diff.
    pub path: String,
    /// Its path in the workspace, or the whole path for a file outside it.
    pub display_path: String,
    /// Whether the session created it.
    pub created: bool,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DiffKind {
    /// The `@@ … @@` line that starts a hunk.
    Hunk,
    Added,
    Removed,
    Context,
    /// A remark such as a rename, a binary file, or a missing line break.
    Note,
}

/// A line of a diff, with its numbers in the old and the new file.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DiffRow {
    pub kind: DiffKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
    pub spans: Vec<Span>,
}

/// A file's diff as a viewer shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DiffView {
    pub path: String,
    pub rows: Vec<DiffRow>,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
    /// Whether the diff was too long for the agent to send whole.
    pub truncated: bool,
    /// The language it was highlighted as, such as `Rust`.
    pub language: Option<String>,
}

impl DiffView {
    fn new(path: &str, lines: Vec<DiffLine>) -> Self {
        let syntax = syntax_for(path, "");
        let rows = diff_rows(lines, syntax);
        let count = |kind| rows.iter().filter(|row| row.kind == kind).count() as u32;
        Self {
            path: path.to_owned(),
            added: count(DiffKind::Added),
            removed: count(DiffKind::Removed),
            rows,
            binary: false,
            truncated: false,
            language: syntax.map(|s| s.name.clone()),
        }
    }
}

/// The rows of a diff, highlighted as the old and the new file: the old
/// file's lines (context and removed ones) are highlighted as one text, and
/// so are the new file's (context and added ones), so a comment or string
/// over several lines comes out right on either side.
fn diff_rows(
    lines: Vec<DiffLine>,
    syntax: Option<&syntect::parsing::SyntaxReference>,
) -> Vec<DiffRow> {
    let (mut old_spans, mut new_spans) = match syntax {
        Some(syntax) => {
            let side = |keep: fn(DiffLineKind) -> bool| {
                let texts: Vec<&str> = lines
                    .iter()
                    .filter(|line| keep(line.kind))
                    .map(|line| line.text.as_str())
                    .collect();
                highlight(syntax, &texts).into_iter()
            };
            (
                Some(side(|k| {
                    matches!(k, DiffLineKind::Context | DiffLineKind::Removed)
                })),
                Some(side(|k| {
                    matches!(k, DiffLineKind::Context | DiffLineKind::Added)
                })),
            )
        }
        None => (None, None),
    };
    let next = |side: &mut Option<std::vec::IntoIter<Vec<Span>>>| {
        side.as_mut().and_then(Iterator::next).unwrap_or_default()
    };
    lines
        .into_iter()
        .map(|line| {
            let (kind, spans) = match line.kind {
                DiffLineKind::Added => (DiffKind::Added, next(&mut new_spans)),
                DiffLineKind::Removed => (DiffKind::Removed, next(&mut old_spans)),
                DiffLineKind::Context => {
                    next(&mut old_spans);
                    (DiffKind::Context, next(&mut new_spans))
                }
                DiffLineKind::Hunk => (DiffKind::Hunk, Vec::new()),
                DiffLineKind::File | DiffLineKind::Note => (DiffKind::Note, Vec::new()),
            };
            DiffRow {
                kind,
                old: line.old,
                new: line.new,
                text: line.text,
                spans,
            }
        })
        .collect()
}

fn session_change(change: &changes::FileChange, workspace: Option<&str>) -> SessionChange {
    SessionChange {
        path: change.path.display().to_string(),
        display_path: match workspace {
            Some(workspace) => display_path(&change.path, Path::new(workspace)),
            None => change.path.display().to_string(),
        },
        created: change.created,
        added: change.added as u32,
        removed: change.removed as u32,
    }
}

#[uniffi::export]
impl AgentConnection {
    /// The workspace's uncommitted changes in git.
    pub async fn git_changes(self: Arc<Self>) -> Result<GitChanges> {
        with_client(&self.shared, move |client| async move {
            Ok(client.workspace_changes().await?.into())
        })
        .await
    }

    /// The diff of a file's uncommitted changes; `old_path` is where a
    /// renamed file came from.
    pub async fn git_diff(
        self: Arc<Self>,
        path: String,
        old_path: Option<String>,
    ) -> Result<DiffView> {
        with_client(&self.shared, move |client| async move {
            let diff = client.file_diff(&path, old_path.as_deref()).await?;
            tokio::task::spawn_blocking(move || {
                let mut view = DiffView::new(&diff.path, diff_lines(&diff.diff));
                view.binary = diff.binary;
                view.truncated = diff.truncated;
                view
            })
            .await
            .map_err(|err| LoomError::invalid(err.to_string()))
        })
        .await
    }

    /// The files an open session changed with its tool calls, in the order it
    /// first changed them. It loads the session's earlier items first, as
    /// the changes may be anywhere in its conversation.
    pub async fn session_changes(self: Arc<Self>, session: String) -> Result<Vec<SessionChange>> {
        let shared = self.shared.clone();
        run(async move {
            let id = SessionId(session);
            shared.load_all(&id).await?;
            let workspace = shared.workspace();
            shared.with_conversation(&id, |conversation| {
                session_changes(&conversation.items)
                    .iter()
                    .map(|change| session_change(change, workspace.as_deref()))
                    .collect()
            })
        })
        .await
    }

    /// What an open session's tool calls changed in a file, one diff after
    /// another, as [`Self::session_changes`] names it.
    pub async fn session_diff(self: Arc<Self>, session: String, path: String) -> Result<DiffView> {
        let shared = self.shared.clone();
        run(async move {
            let id = SessionId(session);
            shared.load_all(&id).await?;
            let workspace = shared.workspace();
            let change = shared
                .with_conversation(&id, |conversation| {
                    session_changes(&conversation.items)
                        .into_iter()
                        .find(|change| change.path == Path::new(&path))
                })?
                .ok_or_else(|| LoomError::invalid(format!("the session did not change {path}")))?;
            let shown = session_change(&change, workspace.as_deref()).display_path;
            tokio::task::spawn_blocking(move || DiffView::new(&shown, change_lines(&change)))
                .await
                .map_err(|err| LoomError::invalid(err.to_string()))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Highlight;

    #[test]
    fn diffs_are_highlighted_as_their_old_and_new_files() {
        let diff = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n\
                    @@ -1,3 +1,3 @@\n fn a() {}\n-/* old\n+/* new\n  still a comment */\n";
        let view = DiffView::new("src/a.rs", diff_lines(diff));
        assert_eq!(view.language.as_deref(), Some("Rust"));
        assert_eq!((view.added, view.removed), (1, 1));
        let kinds: Vec<_> = view.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context
            ]
        );
        assert_eq!(view.rows[1].text, "fn a() {}");
        assert_eq!((view.rows[1].old, view.rows[1].new), (Some(1), Some(1)));
        assert!(
            view.rows[1]
                .spans
                .iter()
                .any(|s| s.highlight == Highlight::Keyword)
        );
        // The comment that the removed and the added line open goes on in
        // the context line after them, on both sides.
        for row in &view.rows[2..] {
            assert_eq!(
                row.spans.first().map(|s| s.highlight),
                Some(Highlight::Comment),
                "{row:?}"
            );
        }
        assert!(view.rows[0].spans.is_empty());
    }

    #[test]
    fn unknown_languages_and_notes_have_no_spans() {
        let diff = "new file mode 100644\n@@ -0,0 +1 @@\n+hello\n\\ No newline at end of file\n";
        let view = DiffView::new("notes.txt", diff_lines(diff));
        assert!(view.language.is_none());
        let kinds: Vec<_> = view.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                DiffKind::Note,
                DiffKind::Hunk,
                DiffKind::Added,
                DiffKind::Note
            ]
        );
        assert!(view.rows.iter().all(|r| r.spans.is_empty()));
    }
}
