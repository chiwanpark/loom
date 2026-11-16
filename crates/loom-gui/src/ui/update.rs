//! Updating the desktop client and its agent from Loom's releases.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    App, AppContext, AsyncWindowContext, Entity, Global, IntoElement, ParentElement, Styled,
    WeakEntity, Window, div,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::notification::Notification;
use gpui_component::progress::Progress;
use gpui_component::{ActiveTheme, WindowExt, v_flex};
use loom_update::{Package, Release, Updater, Version, current_version, install_dir};

use crate::backend::Backend;
use crate::ui::file_view::human_size;
use crate::ui::workspace::Workspace;

/// Tells update notifications apart from others.
struct UpdateNote;

/// Whether a check or an install runs, and the release that was offered
/// last, so that the periodic check does not offer it again. Tests set where
/// the releases are and where the update goes.
#[derive(Default)]
struct UpdateState {
    busy: bool,
    offered: Option<Version>,
    releases: Option<String>,
    dir: Option<PathBuf>,
}

impl Global for UpdateState {}

#[cfg(test)]
fn set_target(releases: String, dir: PathBuf, cx: &mut App) {
    let state = cx.default_global::<UpdateState>();
    state.releases = Some(releases);
    state.dir = Some(dir);
}

fn updater(cx: &mut App) -> Updater {
    match &cx.default_global::<UpdateState>().releases {
        Some(url) => Updater::with_releases_url(url.clone()),
        None => Updater::new(),
    }
}

fn target_dir(cx: &mut App) -> loom_update::Result<PathBuf> {
    match &cx.default_global::<UpdateState>().dir {
        Some(dir) => Ok(dir.clone()),
        None => install_dir(),
    }
}

fn begin(cx: &mut App) -> bool {
    let state = cx.default_global::<UpdateState>();
    !std::mem::replace(&mut state.busy, true)
}

fn end(cx: &mut App) {
    cx.default_global::<UpdateState>().busy = false;
}

/// Looks for a newer release, and offers to install it. A `manual` check
/// also says when there is none or the check failed, and offers a release
/// it offered before.
pub fn check(
    workspace: WeakEntity<Workspace>,
    backend: Backend,
    manual: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if !begin(cx) {
        if manual {
            window.push_notification(
                Notification::info("Loom is updating already").id::<UpdateNote>(),
                cx,
            );
        }
        return;
    }
    let updater = updater(cx);
    let task = backend.run(async move { updater.release(Package::Desktop, None).await });
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                end(cx);
                let current = current_version();
                match result {
                    Some(Ok(release)) if release.version > current => {
                        let state = cx.default_global::<UpdateState>();
                        let seen = state.offered.as_ref() == Some(&release.version);
                        state.offered = Some(release.version.clone());
                        if manual || !seen {
                            offer(workspace, backend, release, window, cx);
                        }
                    }
                    Some(Ok(_)) if manual => window.push_notification(
                        Notification::success(format!("Loom {current} is the latest release"))
                            .id::<UpdateNote>(),
                        cx,
                    ),
                    Some(Ok(_)) | None => {}
                    Some(Err(err)) => {
                        tracing::warn!("could not check for updates: {err}");
                        if manual {
                            window.push_notification(
                                Notification::error(format!("Could not check for updates: {err}"))
                                    .id::<UpdateNote>(),
                                cx,
                            );
                        }
                    }
                }
            });
        })
        .detach();
}

fn offer(
    workspace: WeakEntity<Workspace>,
    backend: Backend,
    release: Release,
    window: &mut Window,
    cx: &mut App,
) {
    let note = Notification::new()
        .id::<UpdateNote>()
        .title(format!("Loom {} is out", release.version))
        .message(format!(
            "This is Loom {}. Update Loom and its agent?",
            current_version()
        ))
        .action(move |_, _, _| {
            let (workspace, backend, release) =
                (workspace.clone(), backend.clone(), release.clone());
            Button::new("install-update")
                .label("Update")
                .primary()
                .on_click(move |_, window, cx| {
                    install(
                        workspace.clone(),
                        backend.clone(),
                        release.clone(),
                        window,
                        cx,
                    )
                })
        });
    window.push_notification(note, cx);
}

/// The bytes of the archive received so far, and the total if known.
struct Download {
    received: u64,
    total: Option<u64>,
}

/// Installs the release where this client is, then offers to restart.
pub fn install(
    workspace: WeakEntity<Workspace>,
    backend: Backend,
    release: Release,
    window: &mut Window,
    cx: &mut App,
) {
    if !begin(cx) {
        return;
    }
    let dir = match target_dir(cx) {
        Ok(dir) => dir,
        Err(err) => {
            end(cx);
            window.push_notification(
                Notification::error(format!("Could not update Loom: {err}")).id::<UpdateNote>(),
                cx,
            );
            return;
        }
    };
    let download = cx.new(|_| Download {
        received: 0,
        total: None,
    });
    window.push_notification(downloading_note(&release, &download), cx);

    let (progress_tx, mut progress) = futures::channel::mpsc::unbounded();
    let updater = updater(cx);
    let task = {
        let release = release.clone();
        let dir = dir.clone();
        backend.run(async move {
            updater
                .install(Package::Desktop, &release, &dir, move |received, total| {
                    let _ = progress_tx.unbounded_send((received, total));
                })
                .await
        })
    };
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            use futures::StreamExt;
            while let Some((received, total)) = progress.next().await {
                download.update(cx, |download, cx| {
                    download.received = received;
                    download.total = total;
                    cx.notify();
                });
            }
            let result = task.await;
            let _ = cx.update(|window, cx| {
                end(cx);
                let note = match result {
                    Some(Ok(_)) => restart_note(workspace, &release, dir.join("loom")),
                    Some(Err(err)) => {
                        tracing::warn!("could not update Loom: {err}");
                        Notification::error(format!("Could not update Loom: {err}"))
                    }
                    None => Notification::error("The update stopped unexpectedly"),
                };
                window.push_notification(note.id::<UpdateNote>(), cx);
            });
        })
        .detach();
}

fn downloading_note(release: &Release, download: &Entity<Download>) -> Notification {
    let observed = Rc::new(Cell::new(false));
    let shown = download.clone();
    Notification::new()
        .id::<UpdateNote>()
        .autohide(false)
        .title(format!("Updating Loom to {}", release.version))
        .content(move |_, _, cx| {
            // Draw the notification again whenever the progress changes.
            if !observed.replace(true) {
                cx.observe(&shown, |_, _, cx| cx.notify()).detach();
            }
            let Download { received, total } = *shown.read(cx);
            let (percent, detail) = match total {
                Some(total) if total > 0 => (
                    received as f32 * 100. / total as f32,
                    format!("{} of {}", human_size(received), human_size(total)),
                ),
                _ => (0., format!("{} downloaded", human_size(received))),
            };
            v_flex()
                .pt_1()
                .gap_1()
                .child(Progress::new("update-progress").value(percent))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                )
                .into_any_element()
        })
}

fn restart_note(
    workspace: WeakEntity<Workspace>,
    release: &Release,
    binary: PathBuf,
) -> Notification {
    Notification::success(
        "Restart Loom to use it. Running agents keep their version until they restart.",
    )
    .title(format!("Loom {} is installed", release.version))
    .action(move |_, _, _| {
        let (workspace, binary) = (workspace.clone(), binary.clone());
        Button::new("restart-after-update")
            .label("Restart")
            .primary()
            .on_click(move |_, window, cx| {
                let binary = binary.clone();
                let _ = workspace.update(cx, |ws, cx| ws.restart(binary, window, cx));
            })
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::Path;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::Root;

    use super::*;
    use crate::test_agent::wait_until;

    /// Serves the files under `root` over HTTP, as the releases page does.
    fn serve(root: PathBuf) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                // The headers end with an empty line.
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                let path = line
                    .split(' ')
                    .nth(1)
                    .unwrap_or("/")
                    .trim_start_matches('/');
                let response = match std::fs::read(root.join(path)) {
                    Ok(body) => {
                        let mut head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        head.extend(body);
                        head
                    }
                    Err(_) => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        url
    }

    /// Lays out release `version` of the desktop client under `root`, with
    /// stubs that print their version.
    fn release(root: &Path, version: &str) {
        let target = Package::Desktop.target().unwrap();
        let stem = format!("loom-{version}-{target}");
        let stage = root.join("stage").join(&stem);
        std::fs::create_dir_all(&stage).unwrap();
        for name in ["loom", "loom-agent"] {
            let path = stage.join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho '{name} {version} (stub)'\n"),
            )
            .unwrap();
            std::process::Command::new("chmod")
                .arg("755")
                .arg(&path)
                .status()
                .unwrap();
        }
        let dir = root.join("latest/download");
        std::fs::create_dir_all(&dir).unwrap();
        let archive = format!("{stem}.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-C")
            .arg(root.join("stage"))
            .arg("-czf")
            .arg(dir.join(&archive))
            .arg(&stem)
            .status()
            .unwrap();
        assert!(status.success());
        let sums = std::process::Command::new("sha256sum")
            .arg(&archive)
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("SHA256SUMS"), sums.stdout).unwrap();
    }

    fn window(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(gpui_component::init);
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|_| gpui::Empty);
            Root::new(view, window, cx)
        });
        cx
    }

    fn busy(cx: &mut App) -> bool {
        cx.default_global::<UpdateState>().busy
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[gpui::test]
    fn a_newer_release_is_offered_and_installed(cx: &mut TestAppContext) {
        let server = tempfile::tempdir().unwrap();
        release(server.path(), "99.0.0");
        let url = serve(server.path().to_path_buf());
        let installed = tempfile::tempdir().unwrap();
        let backend = Backend::new().unwrap();
        let cx = window(cx);
        let dir = installed.path().to_path_buf();
        cx.update(|window, cx| {
            set_target(url, dir, cx);
            check(
                WeakEntity::new_invalid(),
                backend.clone(),
                false,
                window,
                cx,
            );
        });
        wait_until(cx, |cx| {
            !busy(cx) && cx.default_global::<UpdateState>().offered.is_some()
        });
        assert_eq!(
            cx.update(|_, cx| cx.default_global::<UpdateState>().offered.clone()),
            Some(Version::new(99, 0, 0))
        );
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1);

        let updater = cx.update(|_, cx| updater(cx));
        let release = backend.run(async move { updater.release(Package::Desktop, None).await });
        let release = futures::executor::block_on(release).unwrap().unwrap();
        cx.update(|window, cx| {
            install(
                WeakEntity::new_invalid(),
                backend.clone(),
                release,
                window,
                cx,
            )
        });
        wait_until(cx, |_| installed.path().join("loom-agent").exists());
        wait_until(cx, |cx| !busy(cx));
        let version = std::process::Command::new(installed.path().join("loom"))
            .arg("--version")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&version.stdout).trim(),
            "loom 99.0.0 (stub)"
        );
    }

    #[gpui::test]
    fn the_latest_release_is_not_offered(cx: &mut TestAppContext) {
        let Ok(_) = Package::Desktop.target() else {
            return;
        };
        let server = tempfile::tempdir().unwrap();
        release(server.path(), &current_version().to_string());
        let url = serve(server.path().to_path_buf());
        let backend = Backend::new().unwrap();
        let cx = window(cx);
        let dir = server.path().join("unused");
        cx.update(|window, cx| {
            set_target(url, dir, cx);
            check(
                WeakEntity::new_invalid(),
                backend.clone(),
                false,
                window,
                cx,
            );
        });
        cx.executor().allow_parking();
        wait_until(cx, |cx| !busy(cx));
        assert!(cx.update(|_, cx| cx.default_global::<UpdateState>().offered.is_none()));
        assert!(cx.update(|window, cx| window.notifications(cx).is_empty()));
    }
}
