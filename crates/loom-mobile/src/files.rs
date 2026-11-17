//! The files of an agent's workspace: its directories, and its files as
//! highlighted lines for a viewer.

use std::sync::Arc;

use loom_client::proto::{self, SessionId};

use crate::agent::{AgentConnection, with_client};
use crate::error::{LoomError, Result};
use crate::highlight::{CodeLine, code_lines, syntax_for};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum EntryKind {
    File,
    Directory,
    /// A socket, a device, or a symbolic link that leads nowhere.
    Other,
}

impl From<proto::EntryKind> for EntryKind {
    fn from(kind: proto::EntryKind) -> Self {
        match kind {
            proto::EntryKind::File => Self::File,
            proto::EntryKind::Directory => Self::Directory,
            proto::EntryKind::Other => Self::Other,
        }
    }
}

/// A file or directory in a workspace directory.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileEntry {
    pub name: String,
    /// Its path in the workspace, for opening it.
    pub path: String,
    pub kind: EntryKind,
    /// Whether it is a symbolic link; `kind` is what it leads to.
    pub symlink: bool,
    /// The size of a file in bytes.
    pub size: u64,
    /// Whether git ignores it.
    pub ignored: bool,
}

/// A workspace directory: directories first, then files, by name.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DirectoryListing {
    /// Its path in the workspace; empty for the workspace itself.
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// Whether the directory has more entries than the agent lists.
    pub truncated: bool,
}

impl From<proto::DirectoryListing> for DirectoryListing {
    fn from(listing: proto::DirectoryListing) -> Self {
        let path = normal_path(&listing.path);
        let entries = listing
            .entries
            .into_iter()
            .map(|entry| FileEntry {
                path: join(&path, &entry.name),
                name: entry.name,
                kind: entry.kind.into(),
                symlink: entry.symlink,
                size: entry.size,
                ignored: entry.ignored,
            })
            .collect();
        Self {
            path,
            entries,
            truncated: listing.truncated,
        }
    }
}

/// A workspace file as a viewer shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileView {
    pub path: String,
    /// The size of the whole file in bytes.
    pub size: u64,
    /// Whether the file is binary; it then has no lines.
    pub binary: bool,
    /// Whether the file is larger than the agent reads, so `lines` are only
    /// its start.
    pub truncated: bool,
    /// Whether some bytes are not UTF-8 and show as replacement characters.
    pub invalid_utf8: bool,
    /// The name of the language it was highlighted as, such as `Rust`.
    pub language: Option<String>,
    pub lines: Vec<CodeLine>,
}

impl From<proto::FileContent> for FileView {
    fn from(content: proto::FileContent) -> Self {
        let syntax = (!content.binary)
            .then(|| syntax_for(&content.path, &content.text))
            .flatten();
        Self {
            lines: code_lines(&content.text, syntax),
            language: syntax.map(|s| s.name.clone()),
            path: normal_path(&content.path),
            size: content.size,
            binary: content.binary,
            truncated: content.truncated,
            invalid_utf8: !content.binary && !content.valid_utf8,
        }
    }
}

/// A path in the workspace without `./` or a slash at either end; empty for
/// the workspace itself.
fn normal_path(path: &str) -> String {
    let path = path.trim_matches('/');
    let path = path.strip_prefix("./").unwrap_or(path);
    if path == "." {
        String::new()
    } else {
        path.to_owned()
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn checked_path(path: String) -> Result<String> {
    let path = normal_path(&path);
    if path.split('/').any(|part| part == "..") {
        return Err(LoomError::invalid("a path outside the workspace"));
    }
    Ok(path)
}

#[uniffi::export]
impl AgentConnection {
    /// A directory of the agent's workspace, or of the worktree of
    /// `session`; an empty `path` lists the top folder.
    pub async fn list_directory(
        self: Arc<Self>,
        path: String,
        session: Option<String>,
    ) -> Result<DirectoryListing> {
        let path = checked_path(path)?;
        with_client(&self.shared, move |client| async move {
            let client = client.in_session(session.map(SessionId));
            Ok(client.list_directory(&path).await?.into())
        })
        .await
    }

    /// A file of the agent's workspace, or of the worktree of `session`,
    /// highlighted for its language. The agent reads at most the first
    /// 2 MiB of a file.
    pub async fn read_file(
        self: Arc<Self>,
        path: String,
        session: Option<String>,
    ) -> Result<FileView> {
        let path = checked_path(path)?;
        with_client(&self.shared, move |client| async move {
            let content = client
                .in_session(session.map(SessionId))
                .read_file(&path)
                .await?;
            // Highlighting a large file takes a while; it should not hold up
            // the runtime's other work.
            tokio::task::spawn_blocking(move || FileView::from(content))
                .await
                .map_err(|err| LoomError::invalid(err.to_string()))
        })
        .await
    }

    /// Copies a file of the agent's workspace, or of the worktree of
    /// `session`, to `dest` on this device, as for a viewer of images and
    /// PDF files, and returns its size. Fails before anything is written
    /// when the file has more than `max` bytes.
    pub async fn save_file(
        self: Arc<Self>,
        path: String,
        dest: String,
        max: u64,
        session: Option<String>,
    ) -> Result<u64> {
        let path = checked_path(path)?;
        with_client(&self.shared, move |client| async move {
            let dest = std::path::PathBuf::from(dest);
            Ok(client
                .in_session(session.map(SessionId))
                .download_file_within(&path, &dest, max, |_, _| {})
                .await?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_carry_their_workspace_paths() {
        let listing = DirectoryListing::from(proto::DirectoryListing {
            path: "./src/".into(),
            entries: vec![proto::DirEntry {
                name: "main.rs".into(),
                kind: proto::EntryKind::File,
                symlink: false,
                size: 12,
                ignored: false,
            }],
            truncated: false,
        });
        assert_eq!(listing.path, "src");
        assert_eq!(listing.entries[0].path, "src/main.rs");
        let root = DirectoryListing::from(proto::DirectoryListing {
            path: ".".into(),
            ..Default::default()
        });
        assert_eq!(root.path, "");
        assert_eq!(join(&root.path, "a"), "a");
        assert!(checked_path("src/../../etc".into()).is_err());
        assert_eq!(checked_path("/src/".into()).unwrap(), "src");
    }

    #[test]
    fn files_come_highlighted_unless_binary() {
        let view = FileView::from(proto::FileContent {
            path: "src/lib.rs".into(),
            text: "fn a() {}\n".into(),
            size: 10,
            valid_utf8: true,
            ..Default::default()
        });
        assert_eq!(view.language.as_deref(), Some("Rust"));
        assert_eq!(view.lines.len(), 1);
        assert!(!view.lines[0].spans.is_empty());
        assert!(!view.invalid_utf8);
        let binary = FileView::from(proto::FileContent {
            path: "a.png".into(),
            binary: true,
            ..Default::default()
        });
        assert!(binary.lines.is_empty() && binary.language.is_none() && !binary.invalid_utf8);
    }
}
