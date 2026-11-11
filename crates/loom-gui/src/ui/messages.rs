//! The conversation of a session as a virtual list: only the items in view
//! (and a margin around them) are rendered and laid out each frame, so a long
//! session costs no more per frame than a short one.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, FollowMode, HighlightStyle,
    InteractiveElement, IntoElement, ListAlignment, ListState, ObjectFit, ParentElement, Render,
    SharedString, StatefulInteractiveElement, StyleRefinement, Styled, StyledImage, StyledText,
    Subscription, Window, div, img, list, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::text::{TextView, TextViewState};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, Theme, h_flex, v_flex};
use loom_client::proto::{Decision, ToolCall, ToolResultDetails};
use loom_vt::{AlacrittyTerminal, TerminalEmulator};

use crate::attachments;
use crate::conversation::{
    Activity, Conversation, Item, NoticeKind, ToolItem, ToolStatus, tool_label, tool_summary,
};
use crate::ui::code::{self, Highlighted};
use crate::ui::file_view::language;

const OUTPUT_COLS: u16 = 120;
const OUTPUT_ROWS: u16 = 40;
/// How far beyond the viewport items are laid out, so scrolling does not
/// reveal blank space before they are measured.
const OVERDRAW: f32 = 800.;

struct LiveOutput {
    fed: usize,
    emulator: AlacrittyTerminal,
}

/// The parts of an assistant message shown as Markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Part {
    Text,
    Reasoning,
}

pub enum MessagesEvent {
    Resolve { call_id: String, decision: Decision },
}

pub struct MessageList {
    conversation: Entity<Conversation>,
    list: ListState,
    /// The item revisions the list was last told about.
    synced: Vec<u64>,
    expanded: HashSet<String>,
    collapsed: HashSet<String>,
    show_reasoning: HashSet<usize>,
    live: HashMap<String, LiveOutput>,
    /// Highlighted tool arguments and results, by call id, part, and theme.
    /// Both are fixed once they arrive, so they are highlighted only once.
    highlights: HashMap<String, Rc<Highlighted>>,
    /// Parsed Markdown by item and part, with the item revision it was parsed
    /// at. Kept outside the frame so an item scrolled back into view is not
    /// parsed again.
    markdown: HashMap<(usize, Part), (u64, Entity<TextViewState>)>,
    /// How many rows the list rendered, for tests.
    #[cfg(test)]
    rendered_rows: usize,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MessagesEvent> for MessageList {}

impl MessageList {
    pub fn new(conversation: Entity<Conversation>, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&conversation, |this, _, cx| {
                this.sync(cx);
                cx.notify();
            }),
            // Font sizes come from the theme; items out of view keep the heights
            // measured with the old one until they are measured again.
            cx.observe_global::<Theme>(|this, cx| {
                this.list.remeasure();
                cx.notify();
            }),
        ];
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        let mut this = Self {
            conversation,
            list,
            synced: Vec::new(),
            expanded: HashSet::new(),
            collapsed: HashSet::new(),
            show_reasoning: HashSet::new(),
            live: HashMap::new(),
            highlights: HashMap::new(),
            markdown: HashMap::new(),
            #[cfg(test)]
            rendered_rows: 0,
            _subscriptions: subscriptions,
        };
        // The last row is the activity block, empty while the session is idle.
        this.list.splice(0..0, 1);
        this.sync(cx);
        this.list.set_follow_mode(FollowMode::Tail);
        this
    }

    /// Follows new output again, as after sending a prompt.
    pub fn follow(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// Tells the list which rows changed since the last sync, so it measures
    /// them again when they are laid out; rows out of view keep their height
    /// until then.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let revisions = self.conversation.read(cx).item_revisions();
        let common = self.synced.len().min(revisions.len());
        let mut ix = 0;
        while ix < common {
            if self.synced[ix] == revisions[ix] {
                ix += 1;
                continue;
            }
            let start = ix;
            while ix < common && self.synced[ix] != revisions[ix] {
                ix += 1;
            }
            self.list.remeasure_items(start..ix);
        }
        if self.synced.len() != revisions.len() {
            self.list
                .splice(common..self.synced.len(), revisions.len() - common);
        }
        let count = revisions.len();
        // The activity block changes with the session state.
        self.list.remeasure_items(count..count + 1);
        self.markdown.retain(|(ix, _), _| *ix < count);
        self.synced = revisions.to_vec();
    }

    /// Parses the Markdown of an assistant message that is about to be shown,
    /// unless it is already parsed at the item's revision.
    fn prepare_markdown(&mut self, ix: usize, cx: &mut Context<Self>) {
        let conversation = self.conversation.read(cx);
        let Some(Item::Assistant {
            text, reasoning, ..
        }) = conversation.items.get(ix)
        else {
            return;
        };
        let revision = conversation.item_revisions()[ix];
        let mut stale = Vec::new();
        for (part, source, shown) in [
            (Part::Text, text, true),
            (
                Part::Reasoning,
                reasoning,
                self.show_reasoning.contains(&ix),
            ),
        ] {
            let current = self.markdown.get(&(ix, part)).map(|(rev, _)| *rev);
            if shown && !source.is_empty() && current != Some(revision) {
                stale.push((part, source.clone()));
            }
        }
        for (part, source) in stale {
            match self.markdown.get_mut(&(ix, part)) {
                Some((rev, state)) => {
                    *rev = revision;
                    state.update(cx, |state, cx| state.set_text(&source, cx));
                }
                None => {
                    let state = cx.new(|cx| TextViewState::markdown(&source, cx));
                    self.markdown.insert((ix, part), (revision, state));
                }
            }
        }
    }

    fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        {
            self.rendered_rows += 1;
        }
        self.prepare_markdown(ix, cx);
        let conversation = self.conversation.clone();
        let conversation = conversation.read(cx);
        let gap = ix > 0;
        let content = match conversation.items.get(ix) {
            Some(item) => self.render_item(ix, item, cx),
            None => match conversation.activity() {
                Some(activity) => activity_block(&activity, cx.theme()),
                None => return div().into_any_element(),
            },
        };
        div()
            .w_full()
            .when(gap, |el| el.pt_3())
            .child(content)
            .into_any_element()
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

    fn render_item(&mut self, ix: usize, item: &Item, cx: &Context<Self>) -> AnyElement {
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
                                            .when_some(
                                                self.markdown.get(&(ix, Part::Reasoning)),
                                                |el, (_, state)| {
                                                    el.child(TextView::new(state).selectable(true))
                                                },
                                            ),
                                    )
                                }),
                        )
                    })
                    .when_some(
                        self.markdown
                            .get(&(ix, Part::Text))
                            .filter(|_| !text.is_empty()),
                        |el, (_, state)| el.child(TextView::new(state).selectable(true)),
                    )
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
            Item::Tool(tool) => self.render_tool(tool, cx),
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

    fn render_tool(&mut self, tool: &ToolItem, cx: &Context<Self>) -> AnyElement {
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
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(MessagesEvent::Resolve {
                                    call_id: a.clone(),
                                    decision: Decision::Allow,
                                })
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("allow-session-{id}")))
                            .small()
                            .label("Allow for session")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(MessagesEvent::Resolve {
                                    call_id: b.clone(),
                                    decision: Decision::AllowForSession,
                                })
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("deny-{id}")))
                            .danger()
                            .small()
                            .label("Deny")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(MessagesEvent::Resolve {
                                    call_id: c.clone(),
                                    decision: Decision::Deny,
                                })
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

/// The message list, drawn from its own cache: it is rendered again only when
/// it changes, not on every keystroke, cursor blink, or animation frame
/// elsewhere in the window.
///
/// The scrollbar sits out here rather than in the list, because it redraws
/// the view it is in on every frame of its fade and while hovered.
pub fn pane(messages: &Entity<MessageList>, cx: &App) -> impl IntoElement {
    let list = messages.read(cx).list.clone();
    div()
        .relative()
        .flex_1()
        .w_full()
        .min_h_0()
        .child(
            messages
                .clone()
                .cached(StyleRefinement::default().size_full()),
        )
        .vertical_scrollbar(&list)
}

impl Render for MessageList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let conversation = self.conversation.read(cx);
        if conversation.items.is_empty() && !conversation.is_busy() {
            return div()
                .size_full()
                .p_4()
                .text_color(cx.theme().muted_foreground)
                .child("No messages yet. Type a prompt below to start.")
                .into_any_element();
        }
        list(
            self.list.clone(),
            cx.processor(|this, ix, _, cx| this.render_row(ix, cx)),
        )
        .size_full()
        .p_4()
        .into_any_element()
    }
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

#[cfg(test)]
mod tests {
    use super::{CallDetails, call_details, edit_preview, head, split_read_note};
    use loom_client::proto::ToolCall;

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
}

#[cfg(test)]
mod view_tests {
    use std::time::Duration;

    use gpui::{
        AppContext, Context, Entity, IntoElement, Modifiers, ParentElement, Render, ScrollDelta,
        ScrollEvent, Styled, TestAppContext, VisualTestContext, Window, div, point, px,
    };
    use gpui_component::scroll::ScrollbarHandle;
    use gpui_component::v_flex;
    use loom_client::proto::{
        ContentPart, SequencedEvent, SessionEvent, SessionId, SessionInfo, SessionState,
    };

    use super::{MessageList, pane};
    use crate::conversation::Conversation;

    fn info() -> SessionInfo {
        SessionInfo {
            id: SessionId("0123456789abcdef".into()),
            model: None,
            reasoning: None,
            policy_default: None,
            state: SessionState::Idle,
            last_seq: 0,
            created_at: 0,
            title: None,
            archived: false,
        }
    }

    fn apply(conversation: &mut Conversation, event: SessionEvent) {
        let seq = conversation.last_seq + 1;
        conversation.apply(&SequencedEvent { seq, event });
    }

    /// Appends `turns` prompts, each with a few lines of answer.
    fn add_turns(conversation: &mut Conversation, turns: usize) {
        for turn in 0..turns {
            apply(
                conversation,
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text(format!("question {turn}"))],
                },
            );
            apply(
                conversation,
                SessionEvent::AssistantMessage {
                    text: format!("answer {turn}\n\n- one\n- two\n- three"),
                    reasoning: None,
                    model: "test/model".into(),
                },
            );
        }
    }

    /// Stands in for the session view: the message list under a parent that
    /// renders again on its own, as it does on every keystroke in the prompt.
    struct Parent {
        messages: Entity<MessageList>,
    }

    impl Render for Parent {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .child(pane(&self.messages, cx))
                .child(div().h_10().child("prompt"))
        }
    }

    fn open(
        turns: usize,
        cx: &mut TestAppContext,
    ) -> (
        Entity<Conversation>,
        Entity<Parent>,
        Entity<MessageList>,
        &mut VisualTestContext,
    ) {
        cx.update(gpui_component::init);
        let conversation = cx.new(|_| {
            let mut conversation = Conversation::new(info());
            add_turns(&mut conversation, turns);
            conversation
        });
        let messages = cx.new(|cx| MessageList::new(conversation.clone(), cx));
        let (parent, cx) = cx.add_window_view(|_, _| Parent {
            messages: messages.clone(),
        });
        cx.run_until_parked();
        (conversation, parent, messages, cx)
    }

    fn take_rendered(messages: &Entity<MessageList>, cx: &mut VisualTestContext) -> usize {
        messages.update(cx, |messages, _| {
            std::mem::take(&mut messages.rendered_rows)
        })
    }

    #[gpui::test]
    fn a_long_conversation_renders_only_the_rows_in_view(cx: &mut TestAppContext) {
        let (_, _, messages, cx) = open(1000, cx);
        let rendered = take_rendered(&messages, cx);
        assert!(
            rendered > 0 && rendered < 100,
            "rendered {rendered} of 2001 rows"
        );
    }

    #[gpui::test]
    fn a_long_conversation_has_a_scrollbar_over_the_list(cx: &mut TestAppContext) {
        let (_, _, messages, cx) = open(100, cx);
        let overlay = cx.debug_bounds("scrollbar-overlay").expect("no scrollbar");
        messages.read_with(cx, |messages, _| {
            let viewport = ScrollbarHandle::viewport_bounds(&messages.list);
            let content = ScrollbarHandle::content_size(&messages.list);
            assert_eq!(overlay, viewport);
            // Rows out of view count only once measured, but those in view and
            // the overdraw past them are already taller than the viewport.
            assert!(content.height > viewport.size.height, "{content:?}");
        });
    }

    #[gpui::test]
    fn hovering_the_scrollbar_does_not_render_the_list_again(cx: &mut TestAppContext) {
        let (_, _, messages, cx) = open(100, cx);
        let overlay = cx.debug_bounds("scrollbar-overlay").expect("no scrollbar");
        take_rendered(&messages, cx);
        // Wheel over the list, which shows the scrollbar, then rest the
        // pointer on it while it fades in and stays.
        let edge = point(overlay.right() - px(4.), overlay.center().y);
        cx.simulate_event(ScrollEvent {
            position: edge,
            delta: ScrollDelta::Pixels(point(px(0.), px(-200.))),
            ..Default::default()
        });
        cx.run_until_parked();
        take_rendered(&messages, cx);
        for _ in 0..5 {
            cx.simulate_mouse_move(edge, None, Modifiers::default());
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
        }
        assert_eq!(take_rendered(&messages, cx), 0);
    }

    #[gpui::test]
    fn the_list_is_not_rendered_again_when_only_its_parent_is(cx: &mut TestAppContext) {
        let (conversation, parent, messages, cx) = open(20, cx);
        take_rendered(&messages, cx);

        parent.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(take_rendered(&messages, cx), 0);

        conversation.update(cx, |conversation, cx| {
            add_turns(conversation, 1);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(take_rendered(&messages, cx) > 0);
    }

    #[gpui::test]
    fn new_output_stays_in_view_until_the_user_scrolls_away(cx: &mut TestAppContext) {
        let (conversation, _, messages, cx) = open(50, cx);
        let last_row_visible = |messages: &Entity<MessageList>, cx: &mut VisualTestContext| {
            messages.read_with(cx, |messages, cx| {
                let last = messages.conversation.read(cx).items.len() - 1;
                messages.list.bounds_for_item(last).is_some()
            })
        };
        assert!(last_row_visible(&messages, cx));

        conversation.update(cx, |conversation, cx| {
            add_turns(conversation, 5);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(last_row_visible(&messages, cx));

        messages.update(cx, |messages, cx| {
            messages.list.scroll_to(gpui::ListOffset::default());
            cx.notify();
        });
        cx.run_until_parked();
        conversation.update(cx, |conversation, cx| {
            add_turns(conversation, 5);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!last_row_visible(&messages, cx));
        let top = messages.read_with(cx, |messages, _| messages.list.logical_scroll_top());
        assert_eq!(top.item_ix, 0);

        messages.update(cx, |messages, cx| messages.follow(cx));
        cx.run_until_parked();
        assert!(last_row_visible(&messages, cx));
    }

    #[gpui::test]
    fn the_list_tracks_items_that_are_added_and_removed(cx: &mut TestAppContext) {
        let (conversation, _, messages, cx) = open(3, cx);
        let rows = |cx: &mut VisualTestContext| {
            messages.read_with(cx, |messages, _| messages.list.item_count())
        };
        // Six items and the activity row.
        assert_eq!(rows(cx), 7);
        conversation.update(cx, |conversation, cx| {
            apply(
                conversation,
                SessionEvent::AssistantDelta {
                    text: "partial".into(),
                },
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(rows(cx), 8);
        // An empty final message drops the streamed one.
        conversation.update(cx, |conversation, cx| {
            apply(
                conversation,
                SessionEvent::AssistantMessage {
                    text: String::new(),
                    reasoning: None,
                    model: "test/model".into(),
                },
            );
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(rows(cx), 7);
    }
}
