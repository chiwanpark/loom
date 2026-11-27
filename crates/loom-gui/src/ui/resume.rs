use gpui::{AnyWindowHandle, App, AsyncWindowContext, Entity, WeakEntity, Window};
use loom_view_model::codespace_list::{archived_codespace, archived_detail, archived_title};

use crate::state::AgentConnection;
use crate::ui::notify;
use crate::ui::picker::{PickerItem, open_picker};
use crate::ui::workspace::Workspace;

pub fn open(
    conn: Entity<AgentConnection>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let task = conn.read(cx).list_archived_sessions();
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Some(Ok(sessions)) if sessions.is_empty() => {
                    notify::info(window, cx, "This workspace has no archived sessions.")
                }
                Some(Ok(sessions)) => {
                    let now = crate::ui::settings::now_unix();
                    let items = sessions
                        .into_iter()
                        .map(|session| {
                            let (conn, workspace) = (conn.clone(), workspace.clone());
                            let detail = archived_detail(now, &session);
                            PickerItem::new(archived_title(&session), move |window, cx| {
                                resume(
                                    conn.clone(),
                                    workspace.clone(),
                                    window.window_handle(),
                                    session.id.clone(),
                                    archived_codespace(&session),
                                    cx,
                                )
                            })
                            .detail(detail)
                        })
                        .collect();
                    open_picker(
                        "Resume a session",
                        "Find an archived session…",
                        items,
                        window,
                        cx,
                    );
                }
                Some(Err(err)) => notify::error(
                    window,
                    cx,
                    format!("The client cannot list the archived sessions: {err}"),
                ),
                None => {}
            });
        })
        .detach();
}

fn resume(
    conn: Entity<AgentConnection>,
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
    session: loom_client::proto::SessionId,
    codespace: Option<String>,
    cx: &mut App,
) {
    let open_conn = conn.clone();
    conn.update(cx, |conn, cx| {
        conn.resume_session(
            session,
            codespace,
            move |info, cx| {
                let _ = window.update(cx, |_, window, cx| {
                    let _ =
                        workspace.update(cx, |ws, cx| ws.open_session(open_conn, info, window, cx));
                });
            },
            cx,
        )
    });
}
