use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, AsyncApp, AsyncWindowContext, ClipboardEntry, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, FontWeight, Image, InteractiveElement, IntoElement,
    ObjectFit, ParentElement, PathPromptOptions, Render, ScrollEvent, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Subscription, Window, div, img,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{InputEvent, Paste, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::proto::{
    ContentPart, Decision, PolicyAction, ReasoningEffort, Risk, SessionId, SessionState,
    ToolResultDetails,
};
use loom_vt::{AlacrittyTerminal, TerminalEmulator};

use crate::attachments::{self, AttachError};
use crate::conversation::{Conversation, Item, NoticeKind, ToolItem, ToolStatus, tool_summary};
use crate::state::AgentConnection;
use crate::ui::dialogs;
use crate::ui::notify;

const OUTPUT_COLS: u16 = 120;
const OUTPUT_ROWS: u16 = 40;
const RESCROLL_DELAY: std::time::Duration = std::time::Duration::from_millis(120);
/// How close to the bottom a downward scroll must end to resume following.
const FOLLOW_SLACK: f32 = 24.;

/// Whether the message list should keep following new output after a scroll
/// event. `delta_y` is the event's movement (positive scrolls up), and
/// `offset_y` the resulting offset, which gpui has not yet clamped to
/// `-max_y`, so a downward scroll can overshoot past the bottom.
fn follow_after_scroll(follow: bool, delta_y: f32, offset_y: f32, max_y: f32) -> bool {
    if max_y <= 0. {
        // Everything fits; there is nothing to scroll away from.
        return true;
    }
    if delta_y > 0. {
        false
    } else if delta_y < 0. {
        offset_y + max_y <= FOLLOW_SLACK
    } else {
        follow
    }
}

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
    attachments: Vec<Arc<Image>>,
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
            attachments: Vec::new(),
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Renders again shortly after an update, so the list stays at the bottom
    /// when content (images, markdown) settles to its final size a bit later.
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

    fn add_images(
        &mut self,
        images: Vec<Result<Arc<Image>, AttachError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for image in images {
            let checked = image.and_then(|image| {
                attachments::check_total(&self.attachments, &image).map(|()| image)
            });
            match checked {
                Ok(image) => self.attachments.push(image),
                Err(err) => notify::error(window, cx, err.to_string()),
            }
        }
        cx.notify();
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.background_spawn(async move {
            paths
                .iter()
                .map(|path| attachments::from_path(path))
                .collect::<Vec<_>>()
        });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let images = task.await;
            let _ = this.update_in(cx, |this, window, cx| this.add_images(images, window, cx));
        })
        .detach();
    }

    fn pick_images(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(
            window,
            async move |this, cx: &mut AsyncWindowContext| match paths.await {
                Ok(Ok(Some(paths))) => {
                    let _ =
                        this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx));
                }
                Ok(Err(err)) => {
                    let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
                }
                _ => {}
            },
        )
        .detach();
    }

    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let images: Vec<_> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::Image(image) => Some(attachments::from_image(image)),
                _ => None,
            })
            .collect();
        if images.is_empty() || item.text().is_some_and(|t| !t.is_empty()) {
            return;
        }
        cx.stop_propagation();
        self.add_images(images, window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::text(text));
        }
        content.extend(
            std::mem::take(&mut self.attachments)
                .into_iter()
                .map(|image| ContentPart::Image {
                    mime_type: image.format.mime_type().to_owned(),
                    data: image.bytes.clone(),
                }),
        );
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.follow = true;
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.send_prompt(&session, content).await });
        cx.spawn_in(window, async move |_, cx: &mut AsyncWindowContext| {
            if let Some(Err(err)) = task.await {
                let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
            }
        })
        .detach();
    }

    fn compact(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let session = self.session_id(cx);
        let task = conn
            .backend
            .run(async move { client.compact_session(&session).await });
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
            Some(ToolResultDetails::Edit { .. } | ToolResultDetails::WriteWithDiff { .. })
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
            Item::User { text, images } => {
                v_flex()
                    .w_full()
                    .p_3()
                    .rounded_md()
                    .bg(theme.secondary)
                    .border_1()
                    .border_color(theme.border)
                    .gap_2()
                    .when(!text.is_empty(), |el| {
                        el.child(div().whitespace_normal().child(text.clone()))
                    })
                    .when(!images.is_empty(), |el| {
                        el.child(h_flex().flex_wrap().gap_2().children(
                            images.iter().enumerate().map(|(image_ix, image)| {
                                let preview = image.clone();
                                div()
                                    .id(SharedString::from(format!("image-{ix}-{image_ix}")))
                                    .cursor_pointer()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme.border)
                                    .hover(|el| el.border_color(theme.ring))
                                    .on_click(move |_, window, cx| {
                                        crate::ui::image_preview::open_image_preview(
                                            preview.clone(),
                                            window,
                                            cx,
                                        )
                                    })
                                    .child(
                                        img(image.clone())
                                            .max_w(px(480.))
                                            .max_h(px(320.))
                                            .rounded_sm()
                                            .object_fit(ObjectFit::Contain),
                                    )
                            }),
                        ))
                    })
                    .into_any_element()
            }
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
                Some(
                    ToolResultDetails::Edit { diff, .. }
                    | ToolResultDetails::WriteWithDiff { diff, .. },
                ) => Some(diff.clone()),
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

pub fn open_rename(
    connection: Entity<AgentConnection>,
    session: SessionId,
    current: String,
    window: &mut Window,
    cx: &mut App,
) {
    dialogs::open_form(
        "Rename session",
        Some("Leave the title empty to name the session after its first prompt.".into()),
        vec![dialogs::FormField::new("Title", "Session title").value(current)],
        "Rename",
        move |values, _, cx| {
            let title = values.into_iter().next().unwrap_or_default();
            let title = (!title.trim().is_empty()).then_some(title);
            let session = session.clone();
            connection.update(cx, |conn, cx| conn.rename(session, title, cx));
        },
        window,
        cx,
    );
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

pub const REASONING_CHOICES: [(Option<ReasoningEffort>, &str); 7] = [
    (None, "Default"),
    (Some(ReasoningEffort::None), "None"),
    (Some(ReasoningEffort::Minimal), "Minimal"),
    (Some(ReasoningEffort::Low), "Low"),
    (Some(ReasoningEffort::Medium), "Medium"),
    (Some(ReasoningEffort::High), "High"),
    (Some(ReasoningEffort::Xhigh), "Extra high"),
];

pub const POLICY_CHOICES: [(Option<PolicyAction>, &str); 4] = [
    (None, "Default"),
    (Some(PolicyAction::Ask), "Ask"),
    (Some(PolicyAction::Allow), "Allow"),
    (Some(PolicyAction::Deny), "Deny"),
];

pub fn reasoning_label(effort: Option<ReasoningEffort>) -> &'static str {
    REASONING_CHOICES
        .iter()
        .find(|(value, _)| *value == effort)
        .map(|(_, label)| *label)
        .unwrap_or("Default")
}

pub fn policy_label(policy: Option<PolicyAction>) -> &'static str {
    POLICY_CHOICES
        .iter()
        .find(|(value, _)| *value == policy)
        .map(|(_, label)| *label)
        .unwrap_or("Default")
}

impl SessionView {
    fn reasoning_menu(
        &self,
        current: Option<ReasoningEffort>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        Button::new("reasoning")
            .ghost()
            .small()
            .icon(Icon::new(gpui_kit_assets::IconName::Brain))
            .label(format!("Effort: {}", reasoning_label(current)))
            .tooltip("Reasoning effort for this session")
            .dropdown_menu(move |mut menu, _, _| {
                for (value, label) in REASONING_CHOICES {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(value == current)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    let session = this.session_id(cx);
                                    this.connection.update(cx, |conn, cx| {
                                        conn.set_reasoning(session, value, cx)
                                    });
                                });
                            }),
                    );
                }
                menu
            })
    }

    fn policy_menu(
        &self,
        current: Option<PolicyAction>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity().downgrade();
        Button::new("policy")
            .ghost()
            .small()
            .icon(Icon::new(gpui_kit_assets::IconName::ShieldCheck))
            .label(format!("Policy: {}", policy_label(current)))
            .tooltip("Default action for tool calls without a matching policy rule")
            .dropdown_menu(move |mut menu, _, _| {
                for (value, label) in POLICY_CHOICES {
                    let view = view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(value == current)
                            .on_click(move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    let session = this.session_id(cx);
                                    this.connection
                                        .update(cx, |conn, cx| conn.set_policy(session, value, cx));
                                });
                            }),
                    );
                }
                menu
            })
    }
}

impl SessionView {
    fn render_attachments(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .flex_wrap()
            .gap_2()
            .children(self.attachments.iter().enumerate().map(|(ix, image)| {
                let preview = image.clone();
                div()
                    .id(("attachment", ix))
                    .relative()
                    .size(px(96.))
                    .rounded_md()
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .hover(|el| el.border_color(theme.ring))
                    .on_click(move |_, window, cx| {
                        crate::ui::image_preview::open_image_preview(preview.clone(), window, cx)
                    })
                    .child(img(image.clone()).size_full().object_fit(ObjectFit::Cover))
                    .child(
                        div().absolute().top_0().right_0().child(
                            Button::new(("remove-attachment", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Remove")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if ix < this.attachments.len() {
                                        this.attachments.remove(ix);
                                    }
                                    cx.notify();
                                })),
                        ),
                    )
            }))
    }
}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Requested on every render, not when the conversation changes: gpui
        // applies it at the next layout, so the list also stays at the bottom
        // when the viewport shrinks (prompt grows, attachments) or the view is
        // opened on an already loaded conversation, and a stale request cannot
        // pull back a user who has just scrolled up.
        if self.follow {
            self.scroll.scroll_to_bottom();
        }
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
        let compact_tooltip: SharedString = match conversation.last_usage {
            Some(usage) => format!(
                "Summarize earlier turns to free context (now {} tokens)",
                usage.context_tokens()
            )
            .into(),
            None => "Summarize earlier turns to free context".into(),
        };
        let scroll = self.scroll.clone();
        v_flex()
            .id("session")
            .size_full()
            .track_focus(&self.focus)
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.add_paths(paths.paths().to_vec(), window, cx);
            }))
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
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(conversation.title()),
                    )
                    .child(
                        Button::new("rename")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(gpui_kit_assets::IconName::Pencil))
                            .tooltip("Rename session")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let session = this.session_id(cx);
                                let current = this.conversation.read(cx).title();
                                open_rename(this.connection.clone(), session, current, window, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .when(conversation.info.archived, |el| {
                        el.child(
                            Button::new("restore")
                                .ghost()
                                .small()
                                .icon(Icon::new(gpui_kit_assets::IconName::ArchiveRestore))
                                .label("Archived")
                                .tooltip("Restore this session to the list")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let session = this.session_id(cx);
                                    this.connection.update(cx, |conn, cx| {
                                        conn.set_archived(session, false, cx)
                                    });
                                })),
                        )
                    })
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
                    .child(self.reasoning_menu(conversation.info.reasoning, cx))
                    .child(self.policy_menu(conversation.info.policy_default, cx))
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
                    .on_scroll(cx.listener(move |this, event: &ScrollEvent, window, cx| {
                        let delta = event.delta.pixel_delta(window.line_height()).y;
                        this.follow = follow_after_scroll(
                            this.follow,
                            f32::from(delta),
                            f32::from(scroll.offset().y),
                            f32::from(scroll.max_offset().y),
                        );
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
                    .capture_action(cx.listener(Self::on_paste))
                    .when(!self.attachments.is_empty(), |el| {
                        el.child(self.render_attachments(cx))
                    })
                    .child(Textarea::new(&self.input))
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::ImagePlus))
                                    .tooltip("Attach images (or paste or drop them here)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.pick_images(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("compact")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::FoldVertical))
                                    .label("Compact")
                                    .tooltip(compact_tooltip)
                                    .disabled(busy || empty)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.compact(window, cx)),
                                    ),
                            )
                            .child(div().flex_1())
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

#[cfg(test)]
mod tests {
    use super::follow_after_scroll;

    const MAX: f32 = 1000.;

    #[test]
    fn wheel_down_past_the_bottom_keeps_following() {
        // One notch (3 lines) at the bottom leaves the unclamped offset
        // 60 px beyond it; the old `|offset + max| < 24` check stopped here.
        assert!(follow_after_scroll(true, -60., -MAX - 60., MAX));
        assert!(follow_after_scroll(false, -60., -MAX - 60., MAX));
    }

    #[test]
    fn any_upward_scroll_stops_following() {
        // A small trackpad step stays within the slack but must still let
        // the user leave the bottom while output streams in.
        assert!(!follow_after_scroll(true, 5., -MAX + 5., MAX));
        assert!(!follow_after_scroll(true, 300., -MAX + 300., MAX));
    }

    #[test]
    fn scrolling_down_resumes_only_near_the_bottom() {
        assert!(!follow_after_scroll(false, -60., -MAX + 300., MAX));
        assert!(follow_after_scroll(false, -60., -MAX + 20., MAX));
    }

    #[test]
    fn no_movement_or_nothing_to_scroll() {
        assert!(!follow_after_scroll(false, 0., -500., MAX));
        assert!(follow_after_scroll(true, 0., -500., MAX));
        assert!(follow_after_scroll(false, 10., 0., 0.));
    }
}
