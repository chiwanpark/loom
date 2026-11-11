use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, AsyncApp, AsyncWindowContext, ClipboardEntry, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, HighlightStyle, Image, InteractiveElement, IntoElement,
    ObjectFit, ParentElement, PathPromptOptions, Render, ScrollEvent, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, StyledText, Subscription, Window, div, img,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{InputEvent, Paste, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::text::TextView;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::proto::{
    ContentPart, Decision, PolicyAction, ReasoningEffort, SessionId, ToolCall, ToolResultDetails,
    UsageLimit,
};
use loom_vt::{AlacrittyTerminal, TerminalEmulator};

use crate::attachments::{self, AttachError};
use crate::conversation::{
    Activity, Conversation, Item, NoticeKind, ToolItem, ToolStatus, tool_label, tool_summary,
};
use crate::state::{AgentConnection, UsageState};
use crate::ui::code::{self, Highlighted};
use crate::ui::dialogs;
use crate::ui::file_view::language;
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
    /// Highlighted tool arguments and results, by call id, part, and theme.
    /// Both are fixed once they arrive, so they are highlighted only once.
    highlights: HashMap<String, Rc<Highlighted>>,
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
            highlights: HashMap::new(),
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

    fn highlighted(
        &mut self,
        key: String,
        text: &str,
        language: &str,
        theme: &gpui_component::Theme,
    ) -> Rc<Highlighted> {
        // The theme pointer changes when the theme does, which drops stale colors.
        let key = format!("{key}/{:p}", Arc::as_ptr(&theme.highlight_theme));
        self.highlights
            .entry(key)
            .or_insert_with(|| Rc::new(code::highlight(text, language, &theme.highlight_theme)))
            .clone()
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
                                        // `img` sizes itself to the image and clamps
                                        // width and height separately, so a wide image
                                        // under max_w/max_h got letterboxed. Give it
                                        // the scaled size instead.
                                        match attachments::dimensions(image) {
                                            Some(size) => {
                                                let (w, h) = attachments::fit_within(
                                                    size,
                                                    (IMAGE_MAX_W, IMAGE_MAX_H),
                                                );
                                                img(image.clone()).w(px(w)).h(px(h))
                                            }
                                            None => img(image.clone())
                                                .max_w(px(IMAGE_MAX_W))
                                                .max_h(px(IMAGE_MAX_H)),
                                        }
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
                    .child(header_text(&tool.call, &theme)),
            )
            .child(div().text_xs().text_color(color).child(status));

        // Collapsed rows have no body; an empty one would still add its bottom
        // padding under the header.
        let mut has_body = false;
        let mut body = v_flex().w_full().gap_2().px_2().pb_2();
        if expanded {
            if let Some(reason) = &tool.approval_reason
                && tool.status == ToolStatus::WaitingApproval
            {
                has_body = true;
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(theme.warning)
                        .whitespace_normal()
                        .child(reason.clone()),
                );
            }
            let diff = match tool.result.as_ref().and_then(|r| r.details.as_ref()) {
                Some(
                    ToolResultDetails::Edit { diff, .. }
                    | ToolResultDetails::WriteWithDiff { diff, .. },
                ) => Some(diff.clone()),
                _ => None,
            };
            match call_details(&tool.call, diff.is_some()) {
                CallDetails::None => {}
                CallDetails::Diff(text) => {
                    has_body = true;
                    body = body.child(diff_block(&text, &theme));
                }
                CallDetails::Code { text, language } => {
                    let code = self.highlighted(format!("{id}/call"), &text, &language, &theme);
                    has_body = true;
                    body = body.child(code::code_view(&code, None, &theme));
                }
            }
            if let Some(diff) = diff {
                has_body = true;
                body = body.child(diff_block(&diff, &theme));
            } else if let Some(result) = &tool.result {
                if let (
                    Some(ToolResultDetails::Read {
                        path, start_line, ..
                    }),
                    false,
                ) = (&result.details, result.is_error)
                {
                    let (text, note) = split_read_note(&result.content);
                    if !text.is_empty() {
                        let (shown, hidden) = head(text, MAX_BLOCK_LINES);
                        let lang = language(&path.to_string_lossy());
                        let code = self.highlighted(format!("{id}/read"), shown, &lang, &theme);
                        has_body = true;
                        body = body.child(code::code_view(&code, Some(*start_line), &theme));
                        if hidden > 0 {
                            body = body.child(muted_note(
                                format!("… {hidden} more lines not shown"),
                                &theme,
                            ));
                        }
                    }
                    if let Some(note) = note {
                        has_body = true;
                        body = body.child(muted_note(note.to_owned(), &theme));
                    }
                } else if !result.content.trim().is_empty() {
                    has_body = true;
                    body = body.child(code_block(result.content.clone(), &theme));
                }
            } else if !tool.output.is_empty() {
                let text = self.live_text(tool);
                has_body = true;
                body = body.child(code_block(text, &theme));
            }
        }
        if tool.status == ToolStatus::WaitingApproval {
            let (a, b, c) = (id.clone(), id.clone(), id.clone());
            has_body = true;
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
            .when(has_body, |el| el.child(body))
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

/// The color of a tool's name in its header, so each kind of call is easy to
/// pick out.
fn tool_color(name: &str, theme: &gpui_component::Theme) -> gpui::Hsla {
    match name {
        "shell" => theme.magenta,
        "read" => theme.blue,
        "edit" => theme.yellow,
        "write" => theme.cyan,
        _ => theme.foreground,
    }
}

/// The tool summary with the name in the tool's color and the rest of the
/// label (the shell timeout) muted.
fn header_text(call: &ToolCall, theme: &gpui_component::Theme) -> StyledText {
    let (summary, label) = (tool_summary(call), tool_label(call));
    let color = |color| HighlightStyle {
        color: Some(color),
        ..Default::default()
    };
    StyledText::new(summary).with_highlights([
        (0..call.name.len(), color(tool_color(&call.name, theme))),
        (call.name.len()..label.len(), color(theme.muted_foreground)),
    ])
}

/// What to show for a tool call's arguments.
#[derive(Debug, PartialEq, Eq)]
enum CallDetails {
    None,
    Code { text: String, language: String },
    Diff(String),
}

/// Shows each tool's arguments in the form that says the most: the shell
/// command, the content to write, or the edits as a diff. The path is already
/// in the header, and the result's diff replaces the arguments of edits and
/// overwrites once it arrives.
fn call_details(call: &ToolCall, has_result_diff: bool) -> CallDetails {
    let args = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
    let field = |name: &str| {
        args.as_ref()
            .and_then(|args| args.get(name))
            .and_then(|value| value.as_str())
    };
    let code = |text: &str, language: &str| CallDetails::Code {
        text: text.to_owned(),
        language: language.to_owned(),
    };
    match call.name.as_str() {
        "shell" => {
            if let Some(command) = field("command") {
                return code(command, "bash");
            }
        }
        "read" if field("path").is_some() => return CallDetails::None,
        "edit" if has_result_diff => return CallDetails::None,
        "edit" => {
            if let Some(diff) = args.as_ref().and_then(edit_preview) {
                return CallDetails::Diff(diff);
            }
        }
        "write" if has_result_diff => return CallDetails::None,
        "write" => {
            if let (Some(path), Some(content)) = (field("path"), field("content")) {
                let (shown, hidden) = head(content, MAX_BLOCK_LINES);
                let mut text = shown.to_owned();
                if hidden > 0 {
                    text.push_str(&format!("\n… {hidden} more lines not shown"));
                }
                return code(&text, &language(path));
            }
        }
        _ => {}
    }
    if args.is_some() {
        code(&pretty_arguments(&call.arguments), "json")
    } else {
        code(&call.arguments, "text")
    }
}

/// The replacements of an edit call as a diff: removed lines, then added.
fn edit_preview(args: &serde_json::Value) -> Option<String> {
    let text = |value: &serde_json::Value, snake: &str, camel: &str| {
        value
            .get(snake)
            .or_else(|| value.get(camel))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let mut pairs: Vec<(String, String)> = args
        .get("edits")
        .and_then(|edits| edits.as_array())
        .into_iter()
        .flatten()
        .filter_map(|edit| {
            Some((
                text(edit, "old_text", "oldText")?,
                text(edit, "new_text", "newText")?,
            ))
        })
        .collect();
    if let (Some(old), Some(new)) = (
        text(args, "old_text", "oldText"),
        text(args, "new_text", "newText"),
    ) {
        pairs.push((old, new));
    }
    if pairs.is_empty() {
        return None;
    }
    let count = pairs.len();
    let mut out = Vec::new();
    for (ix, (old, new)) in pairs.iter().enumerate() {
        if count > 1 {
            out.push(format!("@@ edit {} of {count} @@", ix + 1));
        }
        out.extend(old.lines().map(|line| format!("-{line}")));
        out.extend(new.lines().map(|line| format!("+{line}")));
    }
    Some(out.join("\n"))
}

/// Splits the read tool's trailing note ("[Showing lines …]", "[File is
/// empty]", "[Line N is …]") from the file content.
fn split_read_note(content: &str) -> (&str, Option<&str>) {
    let content = content.trim_end_matches('\n');
    let (text, last) = match content.rsplit_once('\n') {
        Some((text, last)) => (text, last),
        None => ("", content),
    };
    let is_note = last.ends_with(']')
        && ["[Showing lines ", "[File is empty]", "[Line "]
            .iter()
            .any(|prefix| last.starts_with(prefix));
    if is_note {
        (text.trim_end_matches('\n'), Some(last))
    } else {
        (content, None)
    }
}

/// The first `max` lines of `text`, and how many lines were left out.
fn head(text: &str, max: usize) -> (&str, usize) {
    let total = text.lines().count();
    if total <= max {
        return (text, 0);
    }
    let end = text
        .match_indices('\n')
        .nth(max - 1)
        .map_or(text.len(), |(ix, _)| ix);
    (&text[..end], total - max)
}

fn muted_note(text: String, theme: &gpui_component::Theme) -> AnyElement {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text)
        .into_any_element()
}

const MAX_BLOCK_LINES: usize = 400;

/// The largest size of an image in a prompt.
const IMAGE_MAX_W: f32 = 480.;
const IMAGE_MAX_H: f32 = 320.;

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

/// The last block of a busy conversation: what the session is doing now.
fn activity_block(activity: &Activity, theme: &gpui_component::Theme) -> AnyElement {
    let attention = activity.needs_user();
    let color = if attention {
        theme.warning
    } else {
        theme.muted_foreground
    };
    h_flex()
        .id("activity")
        .w_full()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(if attention {
            theme.warning
        } else {
            theme.border
        })
        .text_sm()
        .text_color(color)
        .child(if attention {
            Icon::new(IconName::TriangleAlert)
                .small()
                .text_color(color)
                .into_any_element()
        } else {
            Spinner::new()
                .icon(IconName::LoaderCircle)
                .small()
                .color(theme.info)
                .into_any_element()
        })
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(activity.label()),
        )
        .into_any_element()
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

/// How close a plan limit is to running out, as in pi-enhanced's status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitLevel {
    Low,
    High,
    Critical,
}

fn limit_level(used_percent: u8) -> LimitLevel {
    match used_percent {
        90.. => LimitLevel::Critical,
        70.. => LimitLevel::High,
        _ => LimitLevel::Low,
    }
}

/// Time left until a reset, in its two largest units: 3d 4h, 2h 13m, 45m.
fn format_until(seconds: u64) -> String {
    let (d, h, m) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (d, h, m) {
        (0, 0, 0) => "under a minute".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// The value shown for a limit: its percentage, or the detail of an uncapped
/// credit balance.
fn limit_value(limit: &UsageLimit) -> String {
    match (limit.used_percent, &limit.detail) {
        (Some(used), _) => format!("{used}%"),
        (None, Some(detail)) => detail.clone(),
        (None, None) => "?".into(),
    }
}

fn limits_tooltip(provider: &str, state: &UsageState, now: u64) -> String {
    let mut lines = vec![format!("Plan usage limits for {provider}")];
    for limit in state.limits.iter().flat_map(|l| &l.limits) {
        let mut line = format!("{}: ", limit.label);
        match (limit.used_percent, &limit.detail) {
            (Some(used), Some(detail)) => line.push_str(&format!("{used}% used ({detail})")),
            (Some(used), None) => line.push_str(&format!("{used}% used")),
            (None, Some(detail)) => line.push_str(detail),
            (None, None) => line.push_str("unknown"),
        }
        if let Some(at) = limit.resets_at {
            line.push_str(&format!(
                ", resets in {}",
                format_until(at.saturating_sub(now))
            ));
        }
        lines.push(line);
    }
    if let Some(error) = &state.error {
        lines.push(format!("Could not refresh: {error}"));
    }
    lines.join("\n")
}

/// Shortens a token count for the status bar: 862, 7.5K, 75.9K, 176K, 1.2M.
pub fn compact_count(n: u64) -> String {
    fn scaled(n: u64, unit: f64, suffix: &str) -> String {
        let value = n as f64 / unit;
        if value < 99.95 {
            let text = format!("{value:.1}");
            format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
        } else {
            format!("{value:.0}{suffix}")
        }
    }
    if n < 1_000 {
        n.to_string()
    } else if n < 999_500 {
        scaled(n, 1e3, "K")
    } else {
        scaled(n, 1e6, "M")
    }
}

/// Groups digits for tooltips: 75862 -> "75,862".
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (ix, ch) in digits.chars().enumerate() {
        if ix > 0 && (digits.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A status bar entry: an icon, a short label, and a tooltip that says what
/// it means.
fn status_item(
    id: &'static str,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    tooltip: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    let tooltip: SharedString = tooltip.into();
    h_flex()
        .id(id)
        .gap_1()
        .child(icon.into().xsmall())
        .child(label.into())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
}

fn limits_item(
    provider: &str,
    state: &UsageState,
    theme: &gpui_component::Theme,
) -> Option<AnyElement> {
    let headline: Vec<&UsageLimit> = match &state.limits {
        Some(limits) => limits.limits.iter().filter(|l| l.headline).collect(),
        None => Vec::new(),
    };
    if headline.is_empty() && state.error.is_none() {
        // Not fetched yet, or the provider has no plan limits.
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tooltip: SharedString = limits_tooltip(provider, state, now).into();
    let mut item = h_flex()
        .id("status-limits")
        .gap_1()
        .child(Icon::new(gpui_kit_assets::IconName::Hourglass).xsmall())
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx));
    if headline.is_empty() {
        return Some(
            item.text_color(theme.warning)
                .child("limits unavailable")
                .into_any_element(),
        );
    }
    for (ix, limit) in headline.into_iter().enumerate() {
        if ix > 0 {
            item = item.child(div().text_color(theme.muted_foreground).child("·"));
        }
        let color = match limit.used_percent.map(limit_level) {
            Some(LimitLevel::Critical) => theme.danger,
            Some(LimitLevel::High) => theme.warning,
            Some(LimitLevel::Low) => theme.success,
            None => theme.muted_foreground,
        };
        item = item
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(limit.label.clone()),
            )
            .child(div().text_color(color).child(limit_value(limit)));
    }
    Some(item.into_any_element())
}

impl SessionView {
    /// The provider whose plan limits belong in the status bar, and a marker
    /// that changes when a turn settles (`None` while one runs).
    pub fn usage_source(&self, cx: &App) -> Option<(String, Option<u64>)> {
        let conv = self.conversation.read(cx);
        let model = conv
            .info
            .model
            .clone()
            .or_else(|| self.connection.read(cx).info.default_model.clone())?;
        let (provider, _) = model.split_once('/')?;
        let total = conv.total_usage;
        let marker = (!conv.is_busy()).then(|| total.prompt_tokens() + total.output);
        Some((provider.to_owned(), marker))
    }

    /// The session's entries in the workspace status bar: model, effort, policy, token usage, plan limits, and pending approvals.
    pub fn status_items(view: &Entity<Self>, cx: &App) -> Vec<AnyElement> {
        let theme = cx.theme();
        let this = view.read(cx);
        let conv = this.conversation.read(cx);
        let weak = view.downgrade();
        let mut items = Vec::new();

        let model = conv
            .info
            .model
            .clone()
            .or_else(|| this.connection.read(cx).info.default_model.clone())
            .unwrap_or_else(|| "no model".into());
        let picker = weak.clone();
        items.push(
            Button::new("status-model")
                .ghost()
                .xsmall()
                .icon(IconName::Bot)
                .label(model)
                .tooltip("Model for this session (click to change)")
                .on_click(move |_, window, cx| {
                    let Some(view) = picker.upgrade() else {
                        return;
                    };
                    let view = view.read(cx);
                    let (connection, session) = (view.connection.clone(), view.session_id(cx));
                    dialogs::open_model_picker(connection, Some(session), window, cx);
                })
                .into_any_element(),
        );

        let current = conv.info.reasoning;
        let menu_view = weak.clone();
        items.push(
            Button::new("status-effort")
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit_assets::IconName::Brain))
                .label(reasoning_label(current))
                .tooltip("Reasoning effort for this session")
                .dropdown_menu(move |mut menu, _, _| {
                    for (value, label) in REASONING_CHOICES {
                        let view = menu_view.clone();
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
                .into_any_element(),
        );

        let current = conv.info.policy_default;
        let menu_view = weak.clone();
        items.push(
            Button::new("status-policy")
                .ghost()
                .xsmall()
                .icon(Icon::new(gpui_kit_assets::IconName::ShieldCheck))
                .label(policy_label(current))
                .tooltip("Default action for tool calls without a matching policy rule")
                .dropdown_menu(move |mut menu, _, _| {
                    for (value, label) in POLICY_CHOICES {
                        let view = menu_view.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .checked(value == current)
                                .on_click(move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        let session = this.session_id(cx);
                                        this.connection.update(cx, |conn, cx| {
                                            conn.set_policy(session, value, cx)
                                        });
                                    });
                                }),
                        );
                    }
                    menu
                })
                .into_any_element(),
        );

        if let Some(usage) = conv.last_usage {
            let context = usage.context_tokens();
            items.push(
                status_item(
                    "status-context",
                    Icon::new(gpui_kit_assets::IconName::Gauge),
                    compact_count(context),
                    format!(
                        "Context: {} tokens after the last response",
                        grouped(context)
                    ),
                )
                .into_any_element(),
            );
            let (input, output) = (conv.total_usage.prompt_tokens(), conv.total_usage.output);
            items.push(
                status_item(
                    "status-usage",
                    Icon::new(gpui_kit_assets::IconName::Coins),
                    format!(
                        "{} in · {} out",
                        compact_count(input),
                        compact_count(output)
                    ),
                    format!(
                        "Tokens used by this session: {} input (cached included), {} output",
                        grouped(input),
                        grouped(output)
                    ),
                )
                .into_any_element(),
            );
        }

        if let Some((provider, _)) = this.usage_source(cx)
            && let Some(state) = this.connection.read(cx).usage.get(&provider)
            && let Some(item) = limits_item(&provider, state, theme)
        {
            items.push(item);
        }

        let pending = conv.pending_approvals().count();
        if pending > 0 {
            items.push(
                status_item(
                    "status-approvals",
                    IconName::TriangleAlert,
                    pending.to_string(),
                    format!("{pending} tool call(s) waiting for approval"),
                )
                .text_color(theme.warning)
                .into_any_element(),
            );
        }

        if conv.info.archived {
            let restore = weak;
            items.push(
                Button::new("status-restore")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(gpui_kit_assets::IconName::ArchiveRestore))
                    .label("Archived")
                    .tooltip("Restore this session to the list")
                    .on_click(move |_, _, cx| {
                        let _ = restore.update(cx, |this, cx| {
                            let session = this.session_id(cx);
                            this.connection
                                .update(cx, |conn, cx| conn.set_archived(session, false, cx));
                        });
                    })
                    .into_any_element(),
            );
        }
        items
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
                compact_count(usage.context_tokens())
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
                            .children(items)
                            .when_some(conversation.activity(), |el, activity| {
                                el.child(activity_block(&activity, &theme))
                            }),
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
                    // Large is the size whose text matches the chat text (1 rem);
                    // the default size uses the smaller `text_sm`.
                    .child(Textarea::new(&self.input).large())
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::ImagePlus))
                                    .label("Attach")
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
                                    .icon(Icon::new(gpui_kit_assets::IconName::CircleStop))
                                    .label("Interrupt")
                                    .tooltip("Stop the current turn")
                                    .disabled(!busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.interrupt(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("send")
                                    .primary()
                                    .small()
                                    .icon(Icon::new(gpui_kit_assets::IconName::SendHorizontal))
                                    .label("Send")
                                    .tooltip("Send the prompt (enter)")
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
    use std::rc::Rc;

    use super::{
        CallDetails, LimitLevel, UsageState, call_details, compact_count, edit_preview,
        follow_after_scroll, format_until, grouped, head, limit_level, limit_value, limits_tooltip,
        split_read_note,
    };
    use loom_client::proto::{ToolCall, UsageLimit};

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".into(),
            name: name.into(),
            arguments: args.to_string(),
            risk: None,
        }
    }

    fn code(text: &str, language: &str) -> CallDetails {
        CallDetails::Code {
            text: text.into(),
            language: language.into(),
        }
    }

    #[test]
    fn shell_shows_the_command() {
        let shell = call(
            "shell",
            serde_json::json!({"command": "cd /x && ls\necho hi", "timeout": 5}),
        );
        assert_eq!(
            call_details(&shell, false),
            code("cd /x && ls\necho hi", "bash")
        );
    }

    #[test]
    fn edit_shows_a_diff_until_the_result_has_one() {
        let edit = call(
            "edit",
            serde_json::json!({"path": "a.rs", "edits": [
                {"old_text": "a\nb", "new_text": "c"},
                {"oldText": "x", "newText": "y\nz"},
            ]}),
        );
        assert_eq!(call_details(&edit, true), CallDetails::None);
        assert_eq!(
            call_details(&edit, false),
            CallDetails::Diff(
                "@@ edit 1 of 2 @@\n-a\n-b\n+c\n@@ edit 2 of 2 @@\n-x\n+y\n+z".into()
            )
        );
        let single = serde_json::json!({"path": "a.rs", "old_text": "a", "new_text": "b"});
        assert_eq!(edit_preview(&single).as_deref(), Some("-a\n+b"));
    }

    #[test]
    fn write_shows_highlighted_content_unless_there_is_a_diff() {
        let write = call(
            "write",
            serde_json::json!({"path": "src/main.rs", "content": "fn main() {}\n"}),
        );
        assert_eq!(call_details(&write, false), code("fn main() {}\n", "rs"));
        assert_eq!(call_details(&write, true), CallDetails::None);
    }

    #[test]
    fn read_has_no_argument_block_and_unknown_tools_show_json() {
        let read = call("read", serde_json::json!({"path": "a.rs", "offset": 3}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let other = call("search", serde_json::json!({"q": "x"}));
        assert_eq!(
            call_details(&other, false),
            code("{\n  \"q\": \"x\"\n}", "json")
        );
    }

    #[test]
    fn read_notes_are_split_from_the_content() {
        assert_eq!(
            split_read_note("a\nb\n\n[Showing lines 1-2 of 9. Use offset=3 to continue.]"),
            (
                "a\nb",
                Some("[Showing lines 1-2 of 9. Use offset=3 to continue.]")
            )
        );
        assert_eq!(
            split_read_note("[File is empty]"),
            ("", Some("[File is empty]"))
        );
        // A TOML file can end with a table header; it is content, not a note.
        assert_eq!(
            split_read_note("a = 1\n[workspace]\n"),
            ("a = 1\n[workspace]", None)
        );
    }

    #[test]
    fn head_keeps_the_first_lines() {
        assert_eq!(head("a\nb\nc\n", 2), ("a\nb", 1));
        assert_eq!(head("a\nb", 2), ("a\nb", 0));
    }

    #[test]
    fn limit_levels_follow_the_reference_thresholds() {
        assert_eq!(limit_level(0), LimitLevel::Low);
        assert_eq!(limit_level(69), LimitLevel::Low);
        assert_eq!(limit_level(70), LimitLevel::High);
        assert_eq!(limit_level(89), LimitLevel::High);
        assert_eq!(limit_level(90), LimitLevel::Critical);
        assert_eq!(limit_level(100), LimitLevel::Critical);
    }

    #[test]
    fn reset_countdowns_use_two_units() {
        assert_eq!(format_until(30), "under a minute");
        assert_eq!(format_until(45 * 60), "45m");
        assert_eq!(format_until(2 * 3600), "2h");
        assert_eq!(format_until(2 * 3600 + 13 * 60 + 5), "2h 13m");
        assert_eq!(format_until(86_400 + 30 * 60), "1d");
        assert_eq!(format_until(3 * 86_400 + 4 * 3600 + 59 * 60), "3d 4h");
    }

    #[test]
    fn limits_tooltip_lists_every_limit_and_the_error() {
        let limit = |label: &str, used, detail: Option<&str>, resets_at| UsageLimit {
            label: label.into(),
            used_percent: used,
            detail: detail.map(str::to_owned),
            resets_at,
            headline: true,
        };
        let mut state = UsageState::default();
        state.limits = Some(Rc::new(loom_client::proto::UsageLimits {
            limits: vec![
                limit("5h", Some(23), None, Some(1000 + 2 * 3600 + 13 * 60)),
                limit("weekly", Some(41), None, None),
                limit("extra usage", Some(12), Some("$12.34 / $100.00"), None),
                limit("credits", None, Some("1,234 left"), None),
            ],
        }));
        state.error = Some("HTTP 429".into());
        assert_eq!(
            limits_tooltip("anthropic", &state, 1000),
            "Plan usage limits for anthropic\n\
             5h: 23% used, resets in 2h 13m\n\
             weekly: 41% used\n\
             extra usage: 12% used ($12.34 / $100.00)\n\
             credits: 1,234 left\n\
             Could not refresh: HTTP 429"
        );
        assert_eq!(
            limit_value(&state.limits.as_ref().unwrap().limits[3]),
            "1,234 left"
        );
    }

    #[test]
    fn compact_counts() {
        let cases = [
            (0, "0"),
            (862, "862"),
            (999, "999"),
            (1_000, "1K"),
            (7_500, "7.5K"),
            (75_862, "75.9K"),
            (99_949, "99.9K"),
            (99_950, "100K"),
            (175_862, "176K"),
            (999_499, "999K"),
            (999_500, "1M"),
            (1_234_567, "1.2M"),
            (250_000_000, "250M"),
        ];
        for (n, text) in cases {
            assert_eq!(compact_count(n), text, "{n}");
        }
    }

    #[test]
    fn grouped_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(75_862), "75,862");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

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
