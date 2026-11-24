use gpui::{AnyElement, Context, IntoElement, ParentElement, Styled, Window, div};
use gpui_component::{ActiveTheme, Disableable, h_flex, v_flex};
use loom_client::oracle_proto::{AuditEntry, AuditLog};
use loom_view_model::settings::{age, audit_action, audit_actor};

use super::{SettingsView, empty, header, now_unix, small_button};

const PAGE: u32 = 100;

#[derive(Default)]
pub(super) struct AuditState {
    pub entries: Vec<AuditEntry>,
    pub more: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

impl SettingsView {
    pub(super) fn load_audit(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) {
        let before = if older {
            self.audit.entries.last().map(|e| e.id)
        } else {
            None
        };
        let task = self.call(cx, move |client| async move {
            client.audit(Some(PAGE), before).await
        });
        if !self.busy.insert("audit".to_owned()) {
            return;
        }
        cx.notify();
        cx.spawn_in(
            window,
            async move |this, cx: &mut gpui::AsyncWindowContext| {
                let result: Result<AuditLog, String> = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.busy.remove("audit");
                    let audit = &mut this.audit;
                    audit.loaded = true;
                    match result {
                        Ok(log) => {
                            audit.error = None;
                            audit.more = log.entries.len() as u32 >= PAGE;
                            if older {
                                audit.entries.extend(log.entries);
                            } else {
                                audit.entries = log.entries;
                            }
                        }
                        Err(err) => audit.error = Some(err),
                    }
                    cx.notify();
                });
            },
        )
        .detach();
    }

    pub(super) fn render_audit(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let loading = self.is_busy("audit");
        let refresh = small_button("refresh-audit", "Refresh")
            .loading(loading)
            .disabled(loading)
            .on_click(cx.listener(|this, _, window, cx| this.load_audit(false, window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_1().child(header(
            "Audit log",
            "The changes that devices, hosts, and the oracle made, newest first.",
            vec![refresh],
            cx,
        ));
        if let Some(error) = &self.audit.error {
            page = page.child(super::danger(error.clone(), cx));
        }
        if self.audit.loaded && self.audit.entries.is_empty() {
            page = page.child(empty("No entries.", cx));
        }
        let now = now_unix();
        for entry in &self.audit.entries {
            page = page.child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .py_0p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(
                        div()
                            .w_24()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(age(now, entry.at)),
                    )
                    .child(div().w_32().flex_none().child(audit_actor(entry)))
                    .child(div().w_48().flex_none().child(audit_action(&entry.action)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .text_color(theme.muted_foreground)
                            .child(entry.detail.clone()),
                    ),
            );
        }
        if self.audit.more {
            page = page.child(
                div().pt_2().child(
                    small_button("older-audit", "Show older entries")
                        .loading(loading)
                        .disabled(loading)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.load_audit(true, window, cx)),
                        ),
                ),
            );
        }
        page.into_any_element()
    }
}
