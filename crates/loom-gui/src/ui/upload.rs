//! Uploading files of this machine to the workspace.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, IntoElement, ParentElement,
    PathPromptOptions, Styled, WeakEntity, Window, div,
};
use gpui_component::button::Button;
use gpui_component::notification::Notification;
use gpui_component::progress::Progress;
use gpui_component::{ActiveTheme, Icon, WindowExt, v_flex};

use crate::state::AgentConnection;
use crate::ui::file_view::human_size;
use crate::ui::files::{FileTreeView, join};
use crate::ui::{dialogs, notify};

/// Tells upload notifications apart from others.
struct UploadNote;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadState {
    Running { sent: u64, total: u64 },
    Done { size: u64 },
    Failed(String),
    Cancelled,
}

pub struct Upload {
    pub name: String,
    /// The workspace path the file goes to.
    pub path: String,
    pub state: UploadState,
    /// Whether the transfer stopped; a cancelled upload settles a moment
    /// after it is cancelled.
    pub settled: bool,
    abort: AbortHandle,
}

impl Upload {
    /// Stops the upload; the agent removes what arrived.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, UploadState::Running { .. }) {
            self.abort.abort();
            self.state = UploadState::Cancelled;
            cx.notify();
        }
    }
}

/// How people call the workspace folder `dir`.
fn folder_name(dir: &str) -> String {
    if dir.is_empty() {
        "the workspace".into()
    } else {
        dir.to_owned()
    }
}

/// Asks for files of this machine, then uploads them to the workspace
/// folder `dir`.
pub fn prompt(
    conn: Entity<AgentConnection>,
    dir: String,
    tree: Option<WeakEntity<FileTreeView>>,
    window: &mut Window,
    cx: &mut App,
) {
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Upload".into()),
    });
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = chosen.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Ok(Some(paths))) => upload_paths(conn, dir, paths, tree, window, cx),
                Ok(Err(err)) => notify::error(window, cx, err.to_string()),
                _ => {}
            });
        })
        .detach();
}

/// Uploads files of this machine to the workspace folder `dir`, after asking
/// whether to replace the ones that are there already. Folders are skipped.
pub fn upload_paths(
    conn: Entity<AgentConnection>,
    dir: String,
    paths: Vec<PathBuf>,
    tree: Option<WeakEntity<FileTreeView>>,
    window: &mut Window,
    cx: &mut App,
) {
    let (files, skipped): (Vec<PathBuf>, Vec<PathBuf>) =
        paths.into_iter().partition(|path| path.is_file());
    if !skipped.is_empty() {
        let names: Vec<String> = skipped
            .iter()
            .map(|path| local_name(path).unwrap_or_else(|| path.display().to_string()))
            .collect();
        notify::error(
            window,
            cx,
            format!("Only files can be uploaded, not {}", names.join(", ")),
        );
    }
    let files: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter_map(|path| local_name(&path).map(|name| (path, name)))
        .collect();
    if files.is_empty() {
        return;
    }
    let listing = conn.read(cx).list_directory(dir.clone());
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let existing: HashSet<String> = match listing.await {
                Some(Ok(listing)) => listing.entries.into_iter().map(|e| e.name).collect(),
                Some(Err(err)) => {
                    let message = format!("Could not upload to {}: {err}", folder_name(&dir));
                    let _ = cx.update(|window, cx| notify::error(window, cx, message));
                    return;
                }
                None => return,
            };
            let _ = cx.update(|window, cx| {
                let taken: Vec<&str> = files
                    .iter()
                    .filter(|(_, name)| existing.contains(name))
                    .map(|(_, name)| name.as_str())
                    .collect();
                if taken.is_empty() {
                    for (local, name) in &files {
                        let path = join(&dir, name);
                        start(
                            conn.clone(),
                            local.clone(),
                            path,
                            false,
                            tree.clone(),
                            window,
                            cx,
                        );
                    }
                    return;
                }
                let description = format!(
                    "{} already in {}. Replace {}?",
                    if taken.len() == 1 {
                        format!("{} is", taken[0])
                    } else {
                        format!("{} are", taken.join(", "))
                    },
                    folder_name(&dir),
                    if taken.len() == 1 { "it" } else { "them" },
                );
                dialogs::confirm(
                    "Replace files?",
                    description,
                    "Replace",
                    move |window, cx| {
                        for (local, name) in &files {
                            let path = join(&dir, name);
                            start(
                                conn.clone(),
                                local.clone(),
                                path,
                                true,
                                tree.clone(),
                                window,
                                cx,
                            );
                        }
                    },
                    window,
                    cx,
                );
            });
        })
        .detach();
}

fn local_name(path: &std::path::Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Uploads the file `local` to the workspace path `path`, showing its
/// progress in a notification that can cancel it, and refreshes `tree` once
/// it is there.
pub fn start(
    conn: Entity<AgentConnection>,
    local: PathBuf,
    path: String,
    overwrite: bool,
    tree: Option<WeakEntity<FileTreeView>>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Upload> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let key = format!("upload-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let (client, backend) = {
        let conn = conn.read(cx);
        (conn.files_client(), conn.backend.clone())
    };
    let (abort, registration) = AbortHandle::new_pair();
    let upload = cx.new(|_| Upload {
        name: local_name(&local).unwrap_or_else(|| path.clone()),
        path: path.clone(),
        state: UploadState::Running { sent: 0, total: 0 },
        settled: false,
        abort,
    });

    let (progress_tx, mut progress) = futures::channel::mpsc::unbounded();
    let result = backend.run(Abortable::new(
        async move {
            client
                .upload_file(&local, &path, overwrite, move |sent, total| {
                    let _ = progress_tx.unbounded_send((sent, total));
                })
                .await
        },
        registration,
    ));
    window.push_notification(running_note(&upload, &key, cx), cx);

    let handle = upload.clone();
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            // The updates stop when the upload ends or is dropped.
            while let Some((sent, total)) = progress.next().await {
                handle.update(cx, |upload, cx| {
                    if let UploadState::Running { .. } = upload.state {
                        upload.state = UploadState::Running { sent, total };
                        cx.notify();
                    }
                });
            }
            let outcome = result.await;
            let _ = cx.update(|window, cx| {
                let state = match outcome {
                    Some(Ok(Ok(size))) => UploadState::Done { size },
                    Some(Ok(Err(err))) => UploadState::Failed(err.to_string()),
                    Some(Err(_aborted)) => UploadState::Cancelled,
                    None => UploadState::Failed("the upload stopped unexpectedly".into()),
                };
                let (name, path) = handle.update(cx, |upload, cx| {
                    if upload.state != UploadState::Cancelled {
                        upload.state = state.clone();
                    }
                    upload.settled = true;
                    cx.notify();
                    (upload.name.clone(), upload.path.clone())
                });
                if matches!(state, UploadState::Done { .. })
                    && let Some(tree) = tree.and_then(|tree| tree.upgrade())
                {
                    tree.update(cx, |tree, cx| tree.refresh(cx));
                }
                finished_note(&state, &name, &path, &key, window, cx);
            });
        })
        .detach();
    upload
}

fn running_note(upload: &Entity<Upload>, key: &str, cx: &App) -> Notification {
    let name = upload.read(cx).name.clone();
    let observed = Rc::new(Cell::new(false));
    let shown = upload.clone();
    let cancel = upload.clone();
    Notification::new()
        .id1::<UploadNote>(key.to_owned())
        .icon(Icon::new(gpui_kit_assets::IconName::Upload))
        .title(format!("Uploading {name}"))
        .content(move |_, _, cx| {
            // Draw the notification again whenever the progress changes.
            if !observed.replace(true) {
                cx.observe(&shown, |_, _, cx| cx.notify()).detach();
            }
            let (sent, total) = match shown.read(cx).state {
                UploadState::Running { sent, total } => (sent, total),
                _ => (0, 0),
            };
            let percent = if total == 0 {
                0.
            } else {
                sent as f32 * 100. / total as f32
            };
            let detail = if total == 0 {
                "Starting…".to_owned()
            } else {
                format!("{} of {}", human_size(sent), human_size(total))
            };
            v_flex()
                .pt_1()
                .gap_1()
                .child(Progress::new("upload-progress").value(percent))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                )
                .into_any_element()
        })
        .action(move |_, _, _| {
            let cancel = cancel.clone();
            Button::new("cancel-upload")
                .label("Cancel")
                .outline()
                .on_click(move |_, _, cx| cancel.update(cx, |upload, cx| upload.cancel(cx)))
        })
}

fn finished_note(
    state: &UploadState,
    name: &str,
    path: &str,
    key: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let note = match state {
        UploadState::Done { size } => {
            Notification::success(format!("Uploaded {name} ({}) to {path}", human_size(*size),))
        }
        UploadState::Failed(err) => {
            tracing::warn!("could not upload {name}: {err}");
            Notification::error(format!("Could not upload {name}: {err}"))
        }
        UploadState::Cancelled | UploadState::Running { .. } => {
            window.remove_notification1::<UploadNote>(key.to_owned(), cx);
            return;
        }
    };
    window.push_notification(note.id1::<UploadNote>(key.to_owned()), cx);
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::Root;

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    fn window<'a>(
        conn: &Entity<AgentConnection>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<FileTreeView>, &'a mut VisualTestContext) {
        let conn = conn.clone();
        let mut tree = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| FileTreeView::new(conn, cx));
            tree = Some(view.clone());
            Root::new(view, window, cx)
        });
        (tree.unwrap(), cx)
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[gpui::test]
    fn a_file_uploads_to_the_workspace(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        std::fs::create_dir(agent.workspace.join("in")).unwrap();
        let local = agent.dir.path().join("local.bin");
        let bytes: Vec<u8> = (0..2_500_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&local, &bytes).unwrap();
        let conn = agent.connect(cx);
        let (tree, cx) = window(&conn, cx);
        let weak = tree.downgrade();

        let upload = cx.update(|window, cx| {
            start(
                conn.clone(),
                local.clone(),
                "in/data.bin".into(),
                false,
                Some(weak),
                window,
                cx,
            )
        });
        assert_eq!(upload.read_with(cx, |u, _| u.name.clone()), "local.bin");
        wait_until(cx, |cx| upload.read(cx).settled);
        assert_eq!(
            upload.read_with(cx, |u, _| u.state.clone()),
            UploadState::Done {
                size: bytes.len() as u64
            }
        );
        assert_eq!(
            std::fs::read(agent.workspace.join("in/data.bin")).unwrap(),
            bytes
        );
        assert_eq!(files_in(&agent.workspace.join("in")), vec!["data.bin"]);
        // The progress notification became the one that reports success.
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1);
    }

    #[gpui::test]
    fn an_upload_does_not_replace_a_file_unless_asked(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        agent.write("a.txt", "old");
        let local = agent.dir.path().join("a.txt");
        std::fs::write(&local, "new").unwrap();
        let conn = agent.connect(cx);
        let (_, cx) = window(&conn, cx);

        let upload = cx.update(|window, cx| {
            start(
                conn.clone(),
                local.clone(),
                "a.txt".into(),
                false,
                None,
                window,
                cx,
            )
        });
        wait_until(cx, |cx| upload.read(cx).settled);
        assert!(matches!(
            upload.read_with(cx, |u, _| u.state.clone()),
            UploadState::Failed(err) if err.contains("already exists")
        ));
        assert_eq!(
            std::fs::read_to_string(agent.workspace.join("a.txt")).unwrap(),
            "old"
        );

        let upload = cx.update(|window, cx| {
            start(
                conn.clone(),
                local.clone(),
                "a.txt".into(),
                true,
                None,
                window,
                cx,
            )
        });
        wait_until(cx, |cx| upload.read(cx).settled);
        assert_eq!(
            std::fs::read_to_string(agent.workspace.join("a.txt")).unwrap(),
            "new"
        );
    }

    #[gpui::test]
    fn chosen_files_upload_to_the_folder(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        std::fs::create_dir(agent.workspace.join("docs")).unwrap();
        let local = agent.dir.path().join("picked");
        std::fs::create_dir(&local).unwrap();
        std::fs::write(local.join("one.txt"), "1").unwrap();
        std::fs::write(local.join("two.txt"), "2").unwrap();
        let conn = agent.connect(cx);
        let (tree, cx) = window(&conn, cx);
        let weak = tree.downgrade();

        cx.update(|window, cx| {
            upload_paths(
                conn.clone(),
                "docs".into(),
                vec![local.join("one.txt"), local.join("two.txt"), local.clone()],
                Some(weak),
                window,
                cx,
            )
        });
        let docs = agent.workspace.join("docs");
        wait_until(cx, |_| files_in(&docs) == vec!["one.txt", "two.txt"]);
        assert_eq!(std::fs::read_to_string(docs.join("two.txt")).unwrap(), "2");
    }

    #[gpui::test]
    fn a_cancelled_upload_leaves_no_file(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        let local = agent.dir.path().join("big.bin");
        std::fs::write(&local, vec![7u8; 8 * 1024 * 1024]).unwrap();
        let conn = agent.connect(cx);
        let (_, cx) = window(&conn, cx);

        let upload = cx.update(|window, cx| {
            let upload = start(
                conn.clone(),
                local.clone(),
                "big.bin".into(),
                false,
                None,
                window,
                cx,
            );
            upload.update(cx, |u, cx| u.cancel(cx));
            upload
        });
        wait_until(cx, |cx| upload.read(cx).settled);
        assert_eq!(
            upload.read_with(cx, |u, _| u.state.clone()),
            UploadState::Cancelled
        );
        wait_until(cx, |_| files_in(&agent.workspace).is_empty());
    }
}
