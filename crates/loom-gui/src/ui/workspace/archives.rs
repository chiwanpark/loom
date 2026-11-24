use gpui::{
    AnyElement, App, AppContext, AsyncWindowContext, ClickEvent, Context, Entity,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::oracle_proto::{Codespace, SessionSummary};
use loom_view_model::codespaces::title;
use loom_view_model::settings::{age, in_time};

use super::{Workspace, notify};
use crate::oracle::OracleLink;
use crate::ui::archive_view::ArchiveView;
use crate::ui::file_view::human_size;

fn session_title(session: &SessionSummary) -> String {
    session
        .title
        .clone()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Untitled session".into())
}

impl Workspace {
    pub(super) fn render_archives(
        &self,
        link: &Entity<OracleLink>,
        (hix, wix): (usize, usize),
        workspace: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let read = link.read(cx);
        let archived: Vec<(Codespace, Vec<SessionSummary>)> = read
            .archived_codespaces(workspace)
            .into_iter()
            .map(|c| (c.clone(), read.codespace_sessions(&c.id)))
            .collect();
        if archived.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let key = (read.name().to_owned(), workspace.to_owned());
        let open = self.open_archives.contains(&key);
        let count = archived.len();
        let mut list = v_flex().w_full().child(
            h_flex()
                .id(SharedString::from(format!("archives-{hix}-{wix}")))
                .debug_selector(move || format!("archives-{hix}-{wix}"))
                .w_full()
                .pl_6()
                .pr_1()
                .gap_1()
                .rounded_md()
                .hover(|s| s.bg(theme.list_hover))
                .cursor_pointer()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall(),
                )
                .child(Icon::new(gpui_kit_assets::IconName::Archive).xsmall())
                .child(format!(
                    "Archive · {count} closed codespace{}",
                    if count == 1 { "" } else { "s" }
                ))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if !this.open_archives.remove(&key) {
                        this.open_archives.insert(key.clone());
                    }
                    cx.notify();
                })),
        );
        if open {
            for (aix, (codespace, sessions)) in archived.into_iter().enumerate() {
                list = list.child(self.render_archived(
                    link,
                    (hix, wix, aix),
                    &codespace,
                    sessions,
                    cx,
                ));
            }
        }
        Some(list.into_any_element())
    }

    fn render_archived(
        &self,
        link: &Entity<OracleLink>,
        (hix, wix, aix): (usize, usize, usize),
        codespace: &Codespace,
        sessions: Vec<SessionSummary>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let now = crate::ui::settings::now_unix();
        let archive = codespace.archive.clone().unwrap_or_default();
        let count = sessions.len();
        let detail = format!(
            "closed {} · {count} session{} · {}",
            age(now, codespace.updated_at),
            if count == 1 { "" } else { "s" },
            human_size(archive.size)
        );
        let mut tooltip = format!("{}\nRead-only sessions", title(codespace));
        if let Some(at) = archive.delete_at {
            tooltip.push_str(&format!(
                "\nThe oracle deletes the archive {}",
                in_time(now, at)
            ));
        }
        let tooltip = SharedString::from(tooltip);
        let newest = sessions.first().cloned();
        let id = codespace.id.clone();
        let click_link = link.clone();
        let menu_link = link.clone();
        let menu_id = codespace.id.clone();
        let this = cx.entity().downgrade();
        h_flex()
            .id(SharedString::from(format!("archived-{hix}-{wix}-{aix}")))
            .debug_selector(move || format!("archived-{hix}-{wix}-{aix}"))
            .w_full()
            .pl_8()
            .pr_1()
            .py_0p5()
            .gap_2()
            .rounded_md()
            .hover(|s| s.bg(theme.list_hover))
            .when(newest.is_some(), |el| el.cursor_pointer())
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if let Some(session) = &newest {
                    this.open_archived(click_link.clone(), &id, session, window, cx);
                }
            }))
            .child(super::dot(theme.muted_foreground))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(codespace.branch.clone()),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!(
                    "archived-menu-{hix}-{wix}-{aix}"
                )))
                .ghost()
                .xsmall()
                .icon(IconName::Ellipsis)
                .tooltip("Archive actions")
                .dropdown_menu(move |mut menu, _, _| {
                    for session in &sessions {
                        let this = this.clone();
                        let link = menu_link.clone();
                        let id = menu_id.clone();
                        let session = session.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format!("Open {}", session_title(&session)))
                                .on_click(
                                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                        let _ = this.update(cx, |ws, cx| {
                                            ws.open_archived(
                                                link.clone(),
                                                &id,
                                                &session,
                                                window,
                                                cx,
                                            )
                                        });
                                    },
                                ),
                        );
                    }
                    let this = this.clone();
                    let link = menu_link.clone();
                    let id = menu_id.clone();
                    menu.separator()
                        .item(PopupMenuItem::new("Delete archive…").on_click(
                            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let _ = this.update(cx, |ws, cx| {
                                    ws.delete_archive(link.clone(), &id, window, cx)
                                });
                            },
                        ))
                }),
            )
            .into_any_element()
    }

    pub(super) fn open_archived(
        &mut self,
        link: Entity<OracleLink>,
        codespace: &str,
        session: &SessionSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self
            .shown_archive
            .as_ref()
            .is_some_and(|view| view.read(cx).is(codespace, &session.id));
        if !shown {
            let codespace = codespace.to_owned();
            let view = cx.new(|cx| ArchiveView::new(link, codespace, session, window, cx));
            self.shown_archive = Some(view);
        }
        self.shown_settings = None;
        if let Some(view) = &self.shown_archive {
            gpui::Focusable::focus_handle(view.read(cx), cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn close_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shown_archive = None;
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    fn delete_archive(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(codespace) = link.read(cx).codespace(id).cloned() else {
            return;
        };
        let this = cx.entity().downgrade();
        let delete_id = codespace.id.clone();
        super::dialogs::confirm(
            "Delete archive",
            format!(
                "Delete the archived sessions of the codespace {}? You cannot undo this.",
                title(&codespace)
            ),
            "Delete archive",
            move |window, cx| {
                let read = link.read(cx);
                let Some(client) = read.client() else {
                    notify::error(window, cx, "The client is not connected to the oracle.");
                    return;
                };
                let id = delete_id.clone();
                let task = read
                    .backend()
                    .run(async move { client.delete_archive(&id).await });
                let (this, id) = (this.clone(), delete_id.clone());
                window
                    .spawn(cx, async move |cx: &mut AsyncWindowContext| {
                        let result = task.await;
                        let _ = cx.update(|window, cx| match result {
                            Some(Ok(())) => {
                                let _ = this.update(cx, |ws, cx| {
                                    let shown = ws
                                        .shown_archive
                                        .as_ref()
                                        .is_some_and(|view| view.read(cx).codespace() == id);
                                    if shown {
                                        ws.close_archive(window, cx);
                                    }
                                });
                            }
                            Some(Err(err)) => notify::error(window, cx, err.to_string()),
                            None => {}
                        });
                    })
                    .detach();
            },
            window,
            cx,
        );
    }

    pub(super) fn render_archive(
        &mut self,
        view: Entity<ArchiveView>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let title = view.read(cx).title();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(gpui_kit_assets::IconName::Archive).small())
                    .child(div().flex_1().min_w_0().text_sm().truncate().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Archived · read-only"),
                    )
                    .child(
                        Button::new("close-archive")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close the archived session")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_archive(window, cx)),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(view))
            .into_any_element()
    }
}
