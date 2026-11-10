use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext, AsyncApp, AsyncWindowContext, Context, Entity, FocusHandle,
    Focusable, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::proto::{Decision, Risk, SessionId, SessionState, ToolResultDetails};
use loom_vt::{AlacrittyTerminal, TerminalEmulator};

use crate::conversation::{Conversation, Item, NoticeKind, ToolItem, ToolStatus, tool_summary};
use crate::state::AgentConnection;
use crate::ui::dialogs;
use crate::ui::notify;

const OUTPUT_COLS: u16 = 120;
const OUTPUT_ROWS: u16 = 40;
const RESCROLL_DELAY: std::time::Duration = std::time::Duration::from_millis(120);

struct LiveOutput {
    fed: usize,
    emulator: AlacrittyTerminal,
}

pub struct SessionView {
    pub connection: Entity<AgentConnection>,
    pub conversation: Entity<Conversation>,
    input: Entity<TextareaState>,
    scroll: ScrollHandle,
    follow: bool,
    rescroll_pending: bool,
    expanded: HashSet<String>,
    collapsed: HashSet<String>,
    show_reasoning: HashSet<usize>,
    live: HashMap<String, LiveOutput>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SessionView {
    pub fn new(
        connection: Entity<AgentConnection>,
        conversation: Entity<Conversation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder("Ask the agent… (Enter to send, Shift+Enter for a new line)")
                .submit_on_enter(true);
            state.set_auto_grow(2, 12, cx);
            state
        });
        let subscriptions = vec![
            cx.observe(&conversation, |this, _, cx| {
                if this.follow {
                    this.scroll.scroll_to_bottom();
                    this.schedule_rescroll(cx);
                }
                cx.notify();
            }),
            cx.observe(&connection, |_, _, cx| cx.notify()),
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift: false, .. } = event {
                    this.submit(window, cx);
                }
            }),
        ];
        Self {
            connection,
            conversation,
            input,
            scroll: ScrollHandle::new(),
            follow: true,
            rescroll_pending: false,
            expanded: HashSet::new(),
            collapsed: HashSet::new(),
            show_reasoning: HashSet::new(),
            live: HashMap::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    fn schedule_rescroll(&mut self, cx: &mut Context<Self>) {
        if self.rescroll_pending {
            return;
        }
        self.rescroll_pending = true;
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            cx.background_executor().timer(RESCROLL_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.rescroll_pending = false;
                if this.follow {
                    this.scroll.scroll_to_bottom();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn session_id(&self, cx: &App) -> SessionId {
        self.conversation.read(cx).info.id.clone()
    }

    pub fn title(&self, cx: &App) -> String {
        self.conversation.read(cx).title()
    }

    pub fn prompt(&self) -> Entity<TextareaState> {
        self.input.clone()
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.follow = true;
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.send_text(&session, &text).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn interrupt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.interrupt(&session).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn resolve(
        &mut self,
        call_id: String,
        decision: Decision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.resolve_approval(&session, &call_id, decision).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn live_text(&mut self, tool: &ToolItem) -> String {
        let live = self
            .live
            .entry(tool.call.call_id.clone())
            .or_insert_with(|| LiveOutput {
                fed: 0,
                emulator: AlacrittyTerminal::new(OUTPUT_COLS, OUTPUT_ROWS),
            });
        if tool.output.len() < live.fed {
            live.fed = 0;
            live.emulator = AlacrittyTerminal::new(OUTPUT_COLS, OUTPUT_ROWS);
        }
        live.emulator.feed(&tool.output[live.fed..]);
        live.fed = tool.output.len();
        let text = live.emulator.snapshot().text();
        text.trim_end().to_owned()
    }

    fn is_expanded(&self, tool: &ToolItem) -> bool {
        let id = &tool.call.call_id;
        if self.expanded.contains(id) {
            return true;
        }
        if self.collapsed.contains(id) {
            return false;
        }
        matches!(
            tool.status,
            ToolStatus::WaitingApproval | ToolStatus::Running | ToolStatus::Failed
        ) || matches!(
            tool.result.as_ref().and_then(|r| r.details.as_ref()),
            Some(ToolResultDetails::Edit { .. })
        )
    }

    fn render_item(
        &mut self,
        ix: usize,
        item: &Item,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        match item {
            Item::User { text, images } => v_flex()
                .w_full()
                .p_3()
                .rounded_md()
                .bg(theme.secondary)
                .border_1()
                .border_color(theme.border)
                .child(div().whitespace_normal().child(text.clone()))
                .when(*images > 0, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{images} image(s) attached")),
                    )
                })
                .into_any_element(),
            Item::Assistant {
                text,
                reasoning,
                streaming,
                model,
            } => {
                let show = self.show_reasoning.contains(&ix);
                v_flex()
                    .w_full()
                    .gap_2()
                    .when(!reasoning.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .gap_1()
                                .items_start()
                                .child(
                                    Button::new(("reasoning", ix))
                                        .ghost()
                                        .xsmall()
                                        .label(if *streaming && text.is_empty() {
                                            "Thinking…"
                                        } else if show {
                                            "Hide reasoning"
                                        } else {
                                            "Show reasoning"
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if !this.show_reasoning.remove(&ix) {
                                                this.show_reasoning.insert(ix);
                                            }
                                            cx.notify();
                                        })),
                                )
                                .when(show, |el| {
                                    el.child(
                                        div()
                                            .pl_3()
                                            .border_l_2()
                                            .border_color(theme.border)
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .child(TextView::markdown(
                                                ("reasoning-text", ix),
                                                reasoning.clone(),
                                            )),
                                    )
                                }),
                        )
                    })
                    .when(!text.is_empty(), |el| {
                        el.child(
                            TextView::markdown(("assistant", ix), text.clone()).selectable(true),
                        )
                    })
                    .when(
                        *streaming && text.is_empty() && reasoning.is_empty(),
                        |el| el.child(div().text_color(theme.muted_foreground).child("…")),
                    )
                    .when_some(model.clone().filter(|_| !*streaming), |el, model| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(model),
                        )
                    })
                    .into_any_element()
            }
            Item::Tool(tool) => self.render_tool(tool, window, cx),
            Item::Notice { kind, text } => {
                let color = match kind {
                    NoticeKind::Info => theme.muted_foreground,
                    NoticeKind::Warning => theme.warning,
                    NoticeKind::Error => theme.danger,
                };
                div()
                    .w_full()
                    .text_sm()
                    .text_color(color)
                    .whitespace_normal()
                    .child(text.clone())
                    .into_any_element()
            }
        }
    }

    fn render_tool(
        &mut self,
        tool: &ToolItem,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = tool.call.call_id.clone();
        let expanded = self.is_expanded(tool);
        let (icon, color, status) = match tool.status {
            ToolStatus::Requested => (IconName::Loader, theme.muted_foreground, "queued"),
            ToolStatus::WaitingApproval => {
                (IconName::TriangleAlert, theme.warning, "needs approval")
            }
            ToolStatus::Running => (IconName::LoaderCircle, theme.info, "running"),
            ToolStatus::Succeeded => (IconName::CircleCheck, theme.success, "done"),
            ToolStatus::Failed => (IconName::CircleX, theme.danger, "failed"),
            ToolStatus::Denied => (IconName::Ban, theme.danger, "denied"),
        };
        let risk = match tool.call.risk {
            Some(Risk::ReadOnly) => Some("read"),
            Some(Risk::Write) => Some("write"),
            Some(Risk::Exec) => Some("exec"),
            None => None,
        };
        let toggle_id = id.clone();
        let header = h_flex()
            .id(SharedString::from(format!("tool-{id}")))
            .w_full()
            .gap_2()
            .px_2()
            .py_1()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let currently = this
                    .conversation
                    .read(cx)
                    .items
                    .iter()
                    .find_map(|i| match i {
                        Item::Tool(t) if t.call.call_id == toggle_id => Some(t.clone()),
                        _ => None,
                    })
                    .map(|t| this.is_expanded(&t))
                    .unwrap_or(false);
                if currently {
                    this.expanded.remove(&toggle_id);
                    this.collapsed.insert(toggle_id.clone());
                } else {
                    this.collapsed.remove(&toggle_id);
                    this.expanded.insert(toggle_id.clone());
                }
                cx.notify();
            }))
            .child(Icon::new(icon).small().text_color(color))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .child(tool_summary(&tool.call)),
            )
            .when_some(risk, |el, risk| {
                el.child(
                    div()
                        .text_xs()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.muted_foreground)
                        .child(risk),
                )
            })
            .child(div().text_xs().text_color(color).child(status));

        let mut body = v_flex().w_full().gap_2().px_2().pb_2();
        if expanded {
            if let Some(reason) = &tool.approval_reason
                && tool.status == ToolStatus::WaitingApproval
            {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(theme.warning)
                        .whitespace_normal()
                        .child(reason.clone()),
                );
            }
            if tool.result.is_none() || tool.call.name != "shell" {
                body = body.child(code_block(pretty_arguments(&tool.call.arguments), &theme));
            }
            let diff = match tool.result.as_ref().and_then(|r| r.details.as_ref()) {
                Some(ToolResultDetails::Edit { diff, .. }) => Some(diff.clone()),
                _ => None,
            };
            if let Some(diff) = diff {
                body = body.child(diff_block(&diff, &theme));
            } else if let Some(result) = &tool.result {
                if !result.content.trim().is_empty() {
                    body = body.child(code_block(result.content.clone(), &theme));
                }
            } else if !tool.output.is_empty() {
                let text = self.live_text(tool);
                body = body.child(code_block(text, &theme));
            }
        }
        if tool.status == ToolStatus::WaitingApproval {
            let (a, b, c) = (id.clone(), id.clone(), id.clone());
            body = body.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("allow-{id}")))
                            .primary()
                            .small()
                            .label("Allow")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.resolve(a.clone(), Decision::Allow, window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("allow-session-{id}")))
                            .small()
                            .label("Allow for session")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.resolve(b.clone(), Decision::AllowForSession, window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("deny-{id}")))
                            .danger()
                            .small()
                            .label("Deny")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.resolve(c.clone(), Decision::Deny, window, cx)
                            })),
                    ),
            );
        }
        v_flex()
            .w_full()
            .rounded_md()
            .border_1()
            .border_color(if tool.status == ToolStatus::WaitingApproval {
                theme.warning
            } else {
                theme.border
            })
            .child(header)
            .child(body)
            .into_any_element()
    }
}

fn pretty_arguments(arguments: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| arguments.to_owned()),
        Err(_) => arguments.to_owned(),
    }
}

const MAX_BLOCK_LINES: usize = 400;

fn clip(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_BLOCK_LINES {
        return text.to_owned();
    }
    let skipped = lines.len() - MAX_BLOCK_LINES;
    format!(
        "… {skipped} earlier lines hidden …\n{}",
        lines[skipped..].join("\n")
    )
}

fn code_block(text: String, theme: &gpui_component::Theme) -> AnyElement {
    div()
        .w_full()
        .p_2()
        .rounded_sm()
        .bg(theme.muted)
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .whitespace_normal()
        .children(
            clip(&text)
                .lines()
                .map(|l| div().child(l.to_owned()))
                .collect::<Vec<_>>(),
        )
        .into_any_element()
}

fn diff_block(diff: &str, theme: &gpui_component::Theme) -> AnyElement {
    let clipped = clip(diff);
    div()
        .w_full()
        .p_2()
        .rounded_sm()
        .bg(theme.muted)
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .children(
            clipped
                .lines()
                .map(|line| {
                    let color = if line.starts_with('+') && !line.starts_with("+++") {
                        Some(theme.green)
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        Some(theme.red)
                    } else if line.starts_with("@@") {
                        Some(theme.cyan)
                    } else {
                        None
                    };
                    div()
                        .when_some(color, |el, c| el.text_color(c))
                        .child(line.to_owned())
                })
                .collect::<Vec<_>>(),
        )
        .into_any_element()
}

fn state_label(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "Idle",
        SessionState::Thinking => "Thinking…",
        SessionState::RunningTool => "Running a tool…",
        SessionState::WaitingApproval => "Waiting for approval",
        SessionState::WaitingLock => "Waiting for the workspace lock",
    }
}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let conversation = self.conversation.read(cx).clone();
        let busy = conversation.is_busy();
        let model = conversation
            .info
            .model
            .clone()
            .or_else(|| self.connection.read(cx).info.default_model.clone())
            .unwrap_or_else(|| "no model".into());
        let items: Vec<AnyElement> = conversation
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| self.render_item(ix, item, window, cx))
            .collect();
        let empty = items.is_empty();
        let scroll = self.scroll.clone();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(conversation.title()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(if busy {
                                theme.info
                            } else {
                                theme.muted_foreground
                            })
                            .child(state_label(conversation.state)),
                    )
                    .child(
                        Button::new("model")
                            .ghost()
                            .small()
                            .label(model)
                            .tooltip("Choose the model for this session")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let session = this.session_id(cx);
                                dialogs::open_model_picker(
                                    this.connection.clone(),
                                    Some(session),
                                    window,
                                    cx,
                                );
                            })),
                    ),
            )
            .child(
                div()
                    .id("messages")
                    .flex_1()
                    .w_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .on_scroll(cx.listener(move |this, _, _, cx| {
                        let offset = scroll.offset().y;
                        let max = scroll.max_offset().y;
                        this.follow = (offset + max).abs() < px(24.);
                        cx.notify();
                    }))
                    .child(
                        v_flex()
                            .w_full()
                            .p_4()
                            .gap_3()
                            .when(empty, |el| {
                                el.child(
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .child("No messages yet. Type a prompt below to start."),
                                )
                            })
                            .children(items),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(Textarea::new(&self.input))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("interrupt")
                                    .small()
                                    .label("Interrupt")
                                    .disabled(!busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.interrupt(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .label("Send")
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                    ),
            )
    }
}
