//! Downloading workspace files to this machine. Only direct connections
//! (this machine or SSH) carry downloads for now; a relay does not.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use futures::future::{AbortHandle, Abortable};
use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, Global, IntoElement, ParentElement,
    Styled, Window, div,
};
use gpui_component::button::Button;
use gpui_component::notification::Notification;
use gpui_component::progress::Progress;
use gpui_component::{ActiveTheme, Icon, WindowExt, v_flex};

use crate::state::AgentConnection;
use crate::ui::file_view::human_size;
use crate::ui::notify;

/// Tells download notifications apart from others.
struct DownloadNote;

/// The folder the last download went to, offered for the next one.
struct LastFolder(PathBuf);

impl Global for LastFolder {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
    Running { received: u64, total: u64 },
    Done { size: u64 },
    Failed(String),
    Cancelled,
}

pub struct Download {
    pub name: String,
    pub dest: PathBuf,
    pub state: DownloadState,
    /// Whether the transfer stopped and a partial file is gone; a cancelled
    /// download settles a moment after it is cancelled.
    pub settled: bool,
    abort: AbortHandle,
}

impl Download {
    /// Stops the download; the partial file is removed.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, DownloadState::Running { .. }) {
            self.abort.abort();
            self.state = DownloadState::Cancelled;
            cx.notify();
        }
    }
}

pub const RELAY_UNSUPPORTED: &str =
    "Downloads need a direct connection; they do not work through a relay yet.";

/// The last part of a workspace path.
fn file_name(relative: &str) -> &str {
    relative.rsplit('/').next().unwrap_or(relative)
}

fn default_folder(cx: &App) -> PathBuf {
    if let Some(LastFolder(dir)) = cx.try_global::<LastFolder>()
        && dir.is_dir()
    {
        return dir.clone();
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}

/// Asks where to save a workspace file, then downloads it there.
pub fn prompt(conn: Entity<AgentConnection>, relative: String, window: &mut Window, cx: &mut App) {
    if !conn.read(cx).downloads_supported() {
        notify::error(window, cx, RELAY_UNSUPPORTED);
        return;
    }
    let chosen = cx.prompt_for_new_path(&default_folder(cx), Some(file_name(&relative)));
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = chosen.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Ok(Some(dest))) => {
                    if let Some(dir) = dest.parent() {
                        cx.set_global(LastFolder(dir.to_path_buf()));
                    }
                    start(conn, relative, dest, window, cx);
                }
                Ok(Err(err)) => notify::error(window, cx, err.to_string()),
                _ => {}
            });
        })
        .detach();
}

/// Downloads a workspace file to `dest`, showing its progress in a
/// notification that can cancel it.
pub fn start(
    conn: Entity<AgentConnection>,
    relative: String,
    dest: PathBuf,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Download> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let key = format!("download-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let (client, backend) = {
        let conn = conn.read(cx);
        (conn.client.clone(), conn.backend.clone())
    };
    let (abort, registration) = AbortHandle::new_pair();
    let download = cx.new(|_| Download {
        name: file_name(&relative).to_owned(),
        dest: dest.clone(),
        state: DownloadState::Running {
            received: 0,
            total: 0,
        },
        settled: false,
        abort,
    });

    let (progress_tx, mut progress) = futures::channel::mpsc::unbounded();
    let result = backend.run(Abortable::new(
        async move {
            client
                .download_file(&relative, &dest, move |received, total| {
                    let _ = progress_tx.unbounded_send((received, total));
                })
                .await
        },
        registration,
    ));
    window.push_notification(running_note(&download, &key, cx), cx);

    let handle = download.clone();
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            // The updates stop when the download ends or is dropped.
            while let Some((received, total)) = progress.next().await {
                handle.update(cx, |download, cx| {
                    if let DownloadState::Running { .. } = download.state {
                        download.state = DownloadState::Running { received, total };
                        cx.notify();
                    }
                });
            }
            let outcome = result.await;
            let _ = cx.update(|window, cx| {
                let state = match outcome {
                    Some(Ok(Ok(size))) => DownloadState::Done { size },
                    Some(Ok(Err(err))) => DownloadState::Failed(err.to_string()),
                    Some(Err(_aborted)) => DownloadState::Cancelled,
                    None => DownloadState::Failed("the download stopped unexpectedly".into()),
                };
                let (name, dest) = handle.update(cx, |download, cx| {
                    if download.state != DownloadState::Cancelled {
                        download.state = state.clone();
                    }
                    download.settled = true;
                    cx.notify();
                    (download.name.clone(), download.dest.clone())
                });
                finished_note(&state, &name, &dest, &key, window, cx);
            });
        })
        .detach();
    download
}

fn running_note(download: &Entity<Download>, key: &str, cx: &App) -> Notification {
    let name = download.read(cx).name.clone();
    let observed = Rc::new(Cell::new(false));
    let shown = download.clone();
    let cancel = download.clone();
    Notification::new()
        .id1::<DownloadNote>(key.to_owned())
        .icon(Icon::new(gpui_kit_assets::IconName::Download))
        .title(format!("Downloading {name}"))
        .content(move |_, _, cx| {
            // Draw the notification again whenever the progress changes.
            if !observed.replace(true) {
                cx.observe(&shown, |_, _, cx| cx.notify()).detach();
            }
            let (received, total) = match shown.read(cx).state {
                DownloadState::Running { received, total } => (received, total),
                _ => (0, 0),
            };
            let percent = if total == 0 {
                0.
            } else {
                received as f32 * 100. / total as f32
            };
            let detail = if total == 0 {
                "Starting…".to_owned()
            } else {
                format!("{} of {}", human_size(received), human_size(total))
            };
            v_flex()
                .pt_1()
                .gap_1()
                .child(Progress::new("download-progress").value(percent))
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
            Button::new("cancel-download")
                .label("Cancel")
                .outline()
                .on_click(move |_, _, cx| cancel.update(cx, |download, cx| download.cancel(cx)))
        })
}

fn finished_note(
    state: &DownloadState,
    name: &str,
    dest: &Path,
    key: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let note = match state {
        DownloadState::Done { size } => Notification::success(format!(
            "Downloaded {name} ({}) to {}",
            human_size(*size),
            dest.display()
        )),
        DownloadState::Failed(err) => {
            tracing::warn!("could not download {name}: {err}");
            Notification::error(format!("Could not download {name}: {err}"))
        }
        DownloadState::Cancelled | DownloadState::Running { .. } => {
            window.remove_notification1::<DownloadNote>(key.to_owned(), cx);
            return;
        }
    };
    window.push_notification(note.id1::<DownloadNote>(key.to_owned()), cx);
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::Root;
    use loom_config::HostEntry;

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};
    use crate::ui::files::FileTreeView;

    fn window<'a>(
        conn: &Entity<AgentConnection>,
        cx: &'a mut TestAppContext,
    ) -> &'a mut VisualTestContext {
        let conn = conn.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let tree = cx.new(|cx| FileTreeView::new(conn, cx));
            Root::new(tree, window, cx)
        });
        cx
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
    fn a_file_downloads_to_the_chosen_path(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        std::fs::create_dir(agent.workspace.join("out")).unwrap();
        let bytes: Vec<u8> = (0..2_500_000u32).map(|i| (i % 253) as u8).collect();
        agent.write("out/data.bin", &bytes);
        let conn = agent.connect(cx);
        let cx = window(&conn, cx);
        let folder = agent.dir.path().join("downloads");
        std::fs::create_dir(&folder).unwrap();
        let dest = folder.join("copy.bin");

        let download = cx.update(|window, cx| {
            start(
                conn.clone(),
                "out/data.bin".into(),
                dest.clone(),
                window,
                cx,
            )
        });
        assert_eq!(download.read_with(cx, |d, _| d.name.clone()), "data.bin");
        wait_until(cx, |cx| download.read(cx).settled);
        assert_eq!(
            download.read_with(cx, |d, _| d.state.clone()),
            DownloadState::Done {
                size: bytes.len() as u64
            }
        );
        assert_eq!(std::fs::read(&dest).unwrap(), bytes);
        assert_eq!(files_in(&folder), vec!["copy.bin"]);
        // The progress notification became the one that reports success.
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1);
    }

    #[gpui::test]
    fn a_failed_download_reports_the_error(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        let conn = agent.connect(cx);
        let cx = window(&conn, cx);
        let folder = agent.dir.path().join("downloads");
        std::fs::create_dir(&folder).unwrap();

        let download = cx.update(|window, cx| {
            start(
                conn.clone(),
                "missing.txt".into(),
                folder.join("x"),
                window,
                cx,
            )
        });
        wait_until(cx, |cx| download.read(cx).settled);
        assert!(matches!(
            download.read_with(cx, |d, _| d.state.clone()),
            DownloadState::Failed(err) if err.contains("missing.txt")
        ));
        assert!(files_in(&folder).is_empty());
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1);
    }

    #[gpui::test]
    fn a_cancelled_download_leaves_no_file(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        agent.write("big.bin", vec![7u8; 8 * 1024 * 1024]);
        let conn = agent.connect(cx);
        let cx = window(&conn, cx);
        let folder = agent.dir.path().join("downloads");
        std::fs::create_dir(&folder).unwrap();

        let download = cx.update(|window, cx| {
            let download = start(
                conn.clone(),
                "big.bin".into(),
                folder.join("big.bin"),
                window,
                cx,
            );
            download.update(cx, |d, cx| d.cancel(cx));
            download
        });
        assert_eq!(
            download.read_with(cx, |d, _| d.state.clone()),
            DownloadState::Cancelled
        );
        wait_until(cx, |cx| download.read(cx).settled);
        assert_eq!(
            download.read_with(cx, |d, _| d.state.clone()),
            DownloadState::Cancelled
        );
        assert!(files_in(&folder).is_empty());
    }

    #[gpui::test]
    fn relayed_agents_offer_no_downloads(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        agent.write("a.txt", "a");
        let local = agent.connect(cx);
        assert!(local.read_with(cx, |conn, _| conn.downloads_supported()));
        let relayed = agent.connect_as(
            HostEntry::Relay {
                name: "relay".into(),
                url: "wss://relay.example".into(),
                host_id: "host".into(),
                host_key: "key".into(),
                instance: None,
                cloudflare_access: None,
            },
            cx,
        );
        assert!(!relayed.read_with(cx, |conn, _| conn.downloads_supported()));
        let cx = window(&relayed, cx);
        // Asking explains why instead of opening a save dialog.
        cx.update(|window, cx| prompt(relayed.clone(), "a.txt".into(), window, cx));
        assert!(!cx.did_prompt_for_new_path());
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1);
    }

    #[gpui::test]
    fn the_save_dialog_picks_the_path_and_its_folder_is_offered_next(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let agent = TestAgent::start();
        agent.write("a.txt", "first");
        let conn = agent.connect(cx);
        let cx = window(&conn, cx);
        let folder = agent.dir.path().join("chosen");
        std::fs::create_dir(&folder).unwrap();

        cx.update(|window, cx| prompt(conn.clone(), "a.txt".into(), window, cx));
        let dest = folder.join("renamed.txt");
        cx.simulate_new_path_selection(|_| Some(dest.clone()));
        wait_until(cx, |_| dest.exists());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "first");

        // Cancelling the dialog downloads nothing.
        cx.update(|window, cx| prompt(conn.clone(), "a.txt".into(), window, cx));
        let mut offered = None;
        cx.simulate_new_path_selection(|dir| {
            offered = Some(dir.to_path_buf());
            None
        });
        cx.run_until_parked();
        assert_eq!(offered.as_deref(), Some(folder.as_path()));
        assert_eq!(files_in(&folder), vec!["renamed.txt"]);
    }
}
