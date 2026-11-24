use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::ButtonVariants;
use gpui_component::{ActiveTheme, h_flex, v_flex};
use loom_client::oracle_proto::{Device, PairingLink, Role};
use loom_view_model::settings::{age, until};

use super::form::{self, Field};
use super::qr::qr_code;
use super::{SettingsView, card, detail, empty, header, now_unix, small_button, subheader};
use crate::ui::notify;

pub struct ShownPairing {
    pub role: Role,
    pub link: PairingLink,
}

impl SettingsView {
    pub(super) fn create_pairing(
        &mut self,
        role: Role,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let task = self.call(cx, move |client| async move {
            match role {
                Role::Host => client.create_host_pairing(None).await,
                _ => client.create_device_pairing(None).await,
            }
        });
        self.act(
            Some(format!("pair:{role}")),
            task,
            move |this, link, _, cx| {
                this.pairing = Some(ShownPairing { role, link });
                cx.notify();
            },
            window,
            cx,
        );
    }

    pub(super) fn cancel_pairing(
        &mut self,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .pairing
            .as_ref()
            .is_some_and(|p| p.link.pairing.id == id)
        {
            self.pairing = None;
        }
        let task = self.call(
            cx,
            move |client| async move { client.cancel_pairing(&id).await },
        );
        self.act(None, task, |_, _, _, _| {}, window, cx);
    }

    pub(super) fn render_pairing(&self, role: Role, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.pairing.as_ref().filter(|p| p.role == role)?;
        let pairing = &shown.link.pairing;
        let used = !self
            .link
            .read(cx)
            .config
            .pairings
            .iter()
            .any(|p| p.id == pairing.id);
        let link = shown.link.link.clone();
        let id = pairing.id.clone();
        let theme = cx.theme().clone();
        let instructions = match role {
            Role::Host => format!(
                "On the host, run: loom-host join '{link}'. Then start loom-host serve as a service."
            ),
            _ => "Open this link in the desktop client of the new device, or scan the QR code with the Android app.".to_owned(),
        };
        let copy = link.clone();
        let mut el = card(cx)
            .debug_selector(|| "settings-pairing".into())
            .child(div().text_sm().whitespace_normal().child(instructions))
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .whitespace_normal()
                    .child(link.clone()),
            );
        if role != Role::Host
            && let Some(qr) = qr_code(&link, 200.)
        {
            el = el.child(qr);
        }
        let status = if used {
            "The link was used or canceled.".to_owned()
        } else {
            format!(
                "The link works once and {}.",
                until(now_unix(), pairing.expires_at)
            )
        };
        Some(
            el.child(detail(status, cx))
                .child(
                    h_flex()
                        .gap_2()
                        .child(small_button("pairing-copy", "Copy link").on_click(
                            move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                notify::info(window, cx, "Copied the link.");
                            },
                        ))
                        .child(
                            small_button(
                                "pairing-done",
                                if used { "Close" } else { "Cancel link" },
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if used {
                                        this.pairing = None;
                                        cx.notify();
                                    } else {
                                        this.cancel_pairing(id.clone(), window, cx);
                                    }
                                },
                            )),
                        ),
                )
                .into_any_element(),
        )
    }

    fn rename_device(&mut self, device: &Device, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.clone();
        let id = device.id;
        form::open(
            "Rename device",
            None,
            vec![Field::text("Name").value(device.label.clone())],
            "Rename",
            move |values, done, window, cx| {
                let label = values.text(0);
                if label.is_empty() {
                    return done.fail("Give a name.", window, cx);
                }
                let task = super::call(&link, cx, move |client| async move {
                    client.rename_device(id, &label).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn revoke_device(&mut self, device: &Device, window: &mut Window, cx: &mut Context<Self>) {
        let id = device.id;
        self.confirm_then(
            format!("Revoke {}?", device.label),
            "The device cannot connect to the oracle or to its codespaces again. To use it again, pair it again.".to_owned(),
            "Revoke",
            move |this, window, cx| {
                let task = this.call(cx, move |client| async move { client.revoke_device(id).await });
                this.act(None, task, |_, _, window, cx| notify::success(window, cx, "Revoked the device."), window, cx);
            },
            window,
            cx,
        );
    }

    pub(super) fn render_devices(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = now_unix();
        let config = self.link.read(cx).config.clone();
        let add = small_button("add-device", "Add device…")
            .loading(self.is_busy("pair:device"))
            .on_click(
                cx.listener(|this, _, window, cx| this.create_pairing(Role::Device, window, cx)),
            )
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Devices",
            "The devices that are paired with this oracle. Each paired device can change all configuration, and it gets the notifications of the codespaces.",
            vec![add],
            cx,
        ));
        if let Some(pairing) = self.render_pairing(Role::Device, cx) {
            page = page.child(pairing);
        }
        if config.devices.is_empty() {
            page = page.child(empty("No devices.", cx));
        }
        for (ix, device) in config.devices.iter().enumerate() {
            let me = config.me == Some(device.id);
            let mut marks = vec![format!("added {}", age(now, device.added_at))];
            if device.connected {
                marks.push("connected".to_owned());
            } else if let Some(seen) = device.last_seen {
                marks.push(format!("last seen {}", age(now, seen)));
            }
            if device.push {
                marks.push("push notifications".to_owned());
            }
            let rename = device.clone();
            let revoke = device.clone();
            page = page.child(
                card(cx)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(device.label.clone()).when(
                                me,
                                |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("this device"),
                                    )
                                },
                            ))
                            .child(
                                small_button(
                                    SharedString::from(format!("rename-device-{ix}")),
                                    "Rename…",
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.rename_device(&rename, window, cx)
                                    },
                                )),
                            )
                            .when(!me, |el| {
                                el.child(
                                    small_button(
                                        SharedString::from(format!("revoke-device-{ix}")),
                                        "Revoke…",
                                    )
                                    .danger()
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.revoke_device(&revoke, window, cx)
                                        },
                                    )),
                                )
                            }),
                    )
                    .child(detail(marks.join(" · "), cx)),
            );
        }
        if !config.pairings.is_empty() {
            page = page.child(subheader("Open pairing links", Vec::new()));
            for (ix, pairing) in config.pairings.iter().enumerate() {
                let id = pairing.id.clone();
                page = page.child(
                    card(cx).child(
                        h_flex()
                            .gap_2()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .child(format!("A link for a {}", pairing.role)),
                                    )
                                    .child(detail(
                                        format!(
                                            "made {}, {}",
                                            age(now, pairing.created_at),
                                            until(now, pairing.expires_at)
                                        ),
                                        cx,
                                    )),
                            )
                            .child(
                                small_button(
                                    SharedString::from(format!("cancel-pairing-{ix}")),
                                    "Cancel",
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.cancel_pairing(id.clone(), window, cx)
                                    },
                                )),
                            ),
                    ),
                );
            }
        }
        page.into_any_element()
    }
}
