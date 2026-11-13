//! The conversation of a session as a virtual list: only the items in view
//! (and a margin around them) are rendered and laid out each frame, so a long
//! session costs no more per frame than a short one.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, AppContext, Context, Div, ElementId, Entity,
    EventEmitter, FollowMode, HighlightStyle, Hsla, InteractiveElement, IntoElement, ListAlignment,
    ListState, ObjectFit, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, StyledImage, StyledText, Subscription, Window, div, img, list,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::scroll::ScrollableElement;
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

/// The padding inside a card (a prompt, a tool call, the status row).
const CARD_PADDING: Pixels = px(12.);
/// Where text starts in every item: a card's padding plus its 1px border.
/// Items without a card are padded by this much, so all text lines up.
const TEXT_INSET: Pixels = px(13.);

/// The space above an item: little between tool calls, so a run of them
/// reads as one block, and the most above a prompt, where a turn starts.
/// `item` is `None` for the status row.
fn gap_before(prev: Option<&Item>, item: Option<&Item>) -> Pixels {
    match (prev, item) {
        (None, _) => px(0.),
        (_, Some(Item::User { .. })) => px(24.),
        (Some(Item::Tool(_)), Some(Item::Tool(_))) => px(4.),
        _ => px(12.),
    }
}

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
    /// The conversation's generation at the last sync.
    generation: u64,
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
            generation: 0,
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
        // The last row is the status row, for what no item shows: thinking
        // before any reasoning streams in, and waiting for the workspace lock.
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
        let conversation = self.conversation.read(cx);
        if conversation.generation != self.generation {
            // Cleared: nothing kept for the old items applies to the new ones.
            self.generation = conversation.generation;
            self.list.splice(0..self.synced.len(), 0);
            self.synced.clear();
            self.expanded.clear();
            self.collapsed.clear();
            self.show_reasoning.clear();
            self.live.clear();
            self.highlights.clear();
            self.markdown.clear();
            self.list.set_follow_mode(FollowMode::Tail);
        }
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
        // The status row changes with the session state.
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
        let gap = gap_before(
            ix.checked_sub(1)
                .and_then(|prev| conversation.items.get(prev)),
            conversation.items.get(ix),
        );
        let content = match conversation.items.get(ix) {
            Some(item) => self.render_item(ix, item, cx),
            None => match conversation.activity() {
                // Once reasoning streams in, its own row says so.
                Some(Activity::Thinking) if !reasoning_streams(conversation) => card(cx.theme())
                    .child(reasoning_row(
                        "Thinking…",
                        "thinking-icon".into(),
                        true,
                        cx.theme(),
                    ))
                    .into_any_element(),
                Some(Activity::WaitingLock) => lock_block(cx.theme()),
                // Everything else shows on the items themselves: the answer as
                // it streams, running tool calls, and calls awaiting approval.
                _ => return div().into_any_element(),
            },
        };
        div().w_full().pt(gap).child(content).into_any_element()
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
        live.emulator
            .feed(&with_carriage_returns(&tool.output[live.fed..]));
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
                    .p(CARD_PADDING)
                    .rounded_md()
                    .bg(theme.secondary)
                    .border_1()
                    .border_color(theme.border)
                    .gap_2()
                    .when(!text.is_empty(), |el| {
                        el.child(
                            div()
                                .debug_selector(|| format!("user-text-{ix}"))
                                .whitespace_normal()
                                .child(text.clone()),
                        )
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
            } => {
                let show = self.show_reasoning.contains(&ix);
                let thinking = *streaming && text.is_empty();
                v_flex()
                    .w_full()
                    .gap_2()
                    .when(!reasoning.is_empty(), |el| {
                        el.child(
                            card(&theme)
                                .id(("reasoning-card", ix))
                                .debug_selector(move || format!("reasoning-card-{ix}"))
                                .hover(|el| el.border_color(theme.ring))
                                .child(
                                    reasoning_row(
                                        if thinking {
                                            "Thinking…"
                                        } else if show {
                                            "Hide reasoning"
                                        } else {
                                            "Show reasoning"
                                        },
                                        format!("reasoning-icon-{ix}"),
                                        thinking,
                                        &theme,
                                    )
                                    .id(("reasoning", ix))
                                    .debug_selector(move || format!("reasoning-row-{ix}"))
                                    .cursor_pointer()
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            if !this.show_reasoning.remove(&ix) {
                                                this.show_reasoning.insert(ix);
                                            }
                                            cx.notify();
                                        },
                                    )),
                                )
                                .when(show, |el| {
                                    // Laid out like the body of a tool call.
                                    el.child(
                                        div()
                                            .px(CARD_PADDING)
                                            .pb_2()
                                            .text_color(theme.muted_foreground)
                                            .when_some(
                                                self.markdown.get(&(ix, Part::Reasoning)),
                                                |el, (_, state)| {
                                                    el.child(
                                                        div()
                                                            .debug_selector(move || {
                                                                format!("reasoning-text-{ix}")
                                                            })
                                                            .child(
                                                                TextView::new(state)
                                                                    .selectable(true),
                                                            ),
                                                    )
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
                        |el, (_, state)| {
                            el.child(
                                div().px(TEXT_INSET).child(
                                    div()
                                        .debug_selector(move || format!("assistant-text-{ix}"))
                                        .child(TextView::new(state).selectable(true)),
                                ),
                            )
                        },
                    )
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
                    .px(TEXT_INSET)
                    .child(
                        div()
                            .debug_selector(move || format!("notice-{ix}"))
                            .text_sm()
                            .text_color(color)
                            .whitespace_normal()
                            .child(text.clone()),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_tool(&mut self, tool: &ToolItem, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = tool.call.call_id.clone();
        let expanded = self.is_expanded(tool);
        // A finished call says so with its icon alone; the other states are
        // worth the word.
        let (icon, color, status) = match tool.status {
            ToolStatus::Requested => (IconName::Loader, theme.muted_foreground, Some("queued")),
            ToolStatus::WaitingApproval => (
                IconName::TriangleAlert,
                theme.warning,
                Some("needs approval"),
            ),
            ToolStatus::Running => (IconName::LoaderCircle, theme.info, Some("running")),
            ToolStatus::Succeeded => (IconName::CircleCheck, theme.success, None),
            ToolStatus::Failed => (IconName::CircleX, theme.danger, Some("failed")),
            ToolStatus::Denied => (IconName::Ban, theme.danger, Some("denied")),
        };
        let toggle_id = id.clone();
        let header = h_flex()
            .id(SharedString::from(format!("tool-{id}")))
            .w_full()
            .gap_2()
            .px(CARD_PADDING)
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
            .child(
                div()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("tool-icon-{id}")
                    })
                    .child(Icon::new(icon).small().text_color(color)),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .map(|el| {
                        if tool.status == ToolStatus::Running {
                            let id = id.clone();
                            el.debug_selector(move || format!("shimmer-{id}"))
                                .child(running_header_text(&tool.call, &theme))
                        } else {
                            el.child(header_text(&tool.call, &theme))
                        }
                    }),
            )
            .when_some(status, |el, status| {
                el.child(div().text_xs().text_color(color).child(status))
            });

        // Collapsed rows have no body; an empty one would still add its bottom
        // padding under the header.
        let mut has_body = false;
        let mut body = v_flex().w_full().gap_2().px(CARD_PADDING).pb_2();
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
        card(&theme)
            .id(SharedString::from(format!("tool-card-{id}")))
            .debug_selector(move || format!("tool-card-{id}"))
            // A call that needs approval keeps its warning border.
            .map(|el| {
                if tool.status == ToolStatus::WaitingApproval {
                    el.border_color(theme.warning)
                } else {
                    el.hover(|el| el.border_color(theme.ring))
                }
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

/// The tool summary with the name in one color for every tool, and the
/// arguments muted so they don't read like the reply text around them. The
/// rest of the label (the shell timeout) is dimmer still.
fn header_text(call: &ToolCall, theme: &gpui_component::Theme) -> StyledText {
    let summary = tool_summary(call);
    let highlights = header_highlights(call, &summary, theme);
    StyledText::new(summary).with_highlights(highlights)
}

/// Puts a carriage return before every line feed, as a terminal's line
/// discipline does. Commands without a terminal write bare line feeds, which
/// move an emulator down a line but not back to its start. Output that has
/// them already gets a second carriage return, which changes nothing.
fn with_carriage_returns(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for &byte in bytes {
        if byte == b'\n' {
            out.push(b'\r');
        }
        out.push(byte);
    }
    out
}

/// The header of a running call: its name shimmers.
fn running_header_text(call: &ToolCall, theme: &Theme) -> impl IntoElement {
    let summary: SharedString = tool_summary(call).into();
    let mut rest = header_highlights(call, &summary, theme);
    // The name's highlight comes first, and the shimmer takes its place.
    rest.remove(0);
    shimmer(
        SharedString::from(format!("shimmer-{}", call.call_id)),
        summary,
        0..call.name.len(),
        rest,
        theme.blue,
        theme.foreground,
    )
}

fn header_highlights(
    call: &ToolCall,
    summary: &str,
    theme: &gpui_component::Theme,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    let label = tool_label(call).len();
    let color = |color| HighlightStyle {
        color: Some(color),
        ..Default::default()
    };
    vec![
        (0..call.name.len(), color(theme.blue)),
        (
            call.name.len()..label,
            color(theme.muted_foreground.opacity(0.7)),
        ),
        (label..summary.len(), color(theme.muted_foreground)),
    ]
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
    let long = |content: &str, language: &str| {
        let (shown, hidden) = head(content, MAX_BLOCK_LINES);
        let mut text = shown.to_owned();
        if hidden > 0 {
            text.push_str(&format!("\n… {hidden} more lines not shown"));
        }
        code(&text, language)
    };
    match call.name.as_str() {
        "shell" => {
            if let Some(command) = field("command") {
                return code(command, "bash");
            }
        }
        "read" if field("path").is_some() => return CallDetails::None,
        "search_web" if field("query").is_some() => return CallDetails::None,
        "edit" if has_result_diff => return CallDetails::None,
        "edit" => {
            if let Some(diff) = args.as_ref().and_then(edit_preview) {
                return CallDetails::Diff(diff);
            }
        }
        "write" if has_result_diff => return CallDetails::None,
        "write" => {
            if let (Some(path), Some(content)) = (field("path"), field("content")) {
                return long(content, &language(path));
            }
        }
        "notes_read" | "tasks_read" => return CallDetails::None,
        "notes_edit" => {
            if let Some(diff) = args.as_ref().and_then(edit_preview) {
                return CallDetails::Diff(diff);
            }
        }
        "notes_write" => {
            if let Some(text) = field("text").or_else(|| field("content")) {
                return long(text, "text");
            }
        }
        "tasks_update" => {
            if let Some(text) = args.as_ref().and_then(task_changes_preview) {
                return long(&text, "text");
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

/// The changes of a tasks_update call, one per line: `+ text` adds a task,
/// `#3 → done` or `#3: text` updates one, `- #3` removes one.
fn task_changes_preview(args: &serde_json::Value) -> Option<String> {
    let changes = args.get("changes")?.as_array()?;
    let mut lines = Vec::new();
    for change in changes {
        let field = |name: &str| change.get(name).and_then(|v| v.as_str());
        let id = match change.get("id") {
            Some(serde_json::Value::Number(n)) => format!("#{n}"),
            Some(serde_json::Value::String(s)) => format!("#{}", s.trim_start_matches('#')),
            _ => "#?".to_owned(),
        };
        let text = field("text").map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "));
        let status = field("status");
        lines.push(match field("action")? {
            "add" => {
                let text = text.unwrap_or_default();
                match status {
                    Some(status) if status != "created" => format!("+ {text} ({status})"),
                    _ => format!("+ {text}"),
                }
            }
            "update" => match (text, status) {
                (Some(text), Some(status)) => format!("{id} → {status}: {text}"),
                (Some(text), None) => format!("{id}: {text}"),
                (None, Some(status)) => format!("{id} → {status}"),
                (None, None) => id,
            },
            "remove" => format!("- {id}"),
            other => format!("{other} {id}"),
        });
    }
    Some(lines.join("\n") + "\n")
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
        .text_size(theme.mono_font_size)
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
        .text_size(theme.mono_font_size)
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

/// Whether the reasoning of the answer being written is streaming in, so
/// its row shows that the model is thinking.
fn reasoning_streams(conversation: &Conversation) -> bool {
    matches!(
        conversation.items.last(),
        Some(Item::Assistant {
            streaming: true,
            reasoning,
            ..
        }) if !reasoning.is_empty()
    )
}

/// The frame of a card: a tool call, or the reasoning of an answer. The
/// cards that can be clicked darken their border while the cursor is over
/// them, which needs an id on the card to keep the hover state.
fn card(theme: &Theme) -> Div {
    v_flex()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
}

/// The header of the card that toggles the reasoning of an answer, laid out
/// like a tool call's header. While the model thinks before any reasoning
/// arrives, the status row shows the same card, so the reasoning takes its
/// place without a jump. A `busy` label shimmers.
fn reasoning_row(label: &'static str, icon_selector: String, busy: bool, theme: &Theme) -> Div {
    h_flex()
        .w_full()
        .gap_2()
        .px(CARD_PADDING)
        .py_1()
        .text_color(theme.muted_foreground)
        .child(
            div()
                .debug_selector(move || icon_selector.clone())
                .child(Icon::new(gpui_kit_assets::IconName::Brain).small()),
        )
        .map(|el| {
            if busy {
                el.child(shimmering_label("shimmer-thinking", label, theme))
            } else {
                el.child(label)
            }
        })
}

/// The status row while the session waits for another one to release the
/// workspace, which no item shows.
fn lock_block(theme: &Theme) -> AnyElement {
    h_flex()
        .id("activity")
        .debug_selector(|| "status-block".into())
        .w_full()
        .gap_2()
        .px(CARD_PADDING)
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(
            Icon::new(IconName::LoaderCircle)
                .small()
                .text_color(theme.info),
        )
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(shimmering_label(
                    "shimmer-lock",
                    Activity::WaitingLock.label(),
                    theme,
                )),
        )
        .into_any_element()
}

/// How often the shimmer crosses its text.
const SHIMMER_PERIOD: Duration = Duration::from_millis(2000);
/// The part of the period the band spends crossing; it rests the rest.
const SHIMMER_SWEEP: f32 = 0.7;
/// How far the band moves a color toward the foreground at its peak.
const SHIMMER_STRENGTH: f32 = 0.8;
/// A color sweep looks smooth at this rate, and the list it is in redraws
/// only half as often as on every frame.
const SHIMMER_FPS: f32 = 30.;

/// How much the band brightens character `ix` of `len` at `delta` of the
/// loop, from 0 to 1. The band is about a third of the text wide, so short
/// and long texts both get one whole sweep.
fn shimmer_strength(len: usize, ix: usize, delta: f32) -> f32 {
    let sweep = delta / SHIMMER_SWEEP;
    if !(0.0..1.0).contains(&sweep) {
        return 0.0;
    }
    let half = (len as f32 * 0.35).max(1.5);
    let center = -half + (len as f32 + 2.0 * half) * sweep;
    let t = (1.0 - ((ix as f32 + 0.5) - center).abs() / half).max(0.0);
    t * t * (3.0 - 2.0 * t)
}

/// Colors each character of `text[range]` with `base`, moved toward `peak`
/// where the band is at `delta` of the loop. Neighbours of the same color
/// share a run.
fn shimmer_highlights(
    text: &str,
    range: std::ops::Range<usize>,
    base: Hsla,
    peak: Hsla,
    delta: f32,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    let part = &text[range.clone()];
    let len = part.chars().count();
    let mut runs: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    for (ix, (offset, ch)) in part.char_indices().enumerate() {
        let start = range.start + offset;
        let end = start + ch.len_utf8();
        let strength = shimmer_strength(len, ix, delta);
        let color = if strength > 0.0 {
            base.blend(peak.opacity(SHIMMER_STRENGTH * strength))
        } else {
            base
        };
        match runs.last_mut() {
            Some((run, style)) if style.color == Some(color) => run.end = end,
            _ => runs.push((
                start..end,
                HighlightStyle {
                    color: Some(color),
                    ..Default::default()
                },
            )),
        }
    }
    runs
}

/// Text that shows a block is busy: a band of stronger color sweeps across
/// `range` of it now and then. `rest` styles the text after the range.
/// Every shimmer moves in step, and with reduced motion the text is still.
fn shimmer(
    id: impl Into<ElementId>,
    text: SharedString,
    range: std::ops::Range<usize>,
    rest: Vec<(std::ops::Range<usize>, HighlightStyle)>,
    base: Hsla,
    peak: Hsla,
) -> impl IntoElement {
    StyledText::new(text.clone()).with_animation(
        id,
        Animation::new(SHIMMER_PERIOD)
            .repeat_synced()
            .with_max_fps(SHIMMER_FPS),
        move |_, delta| {
            let mut highlights = shimmer_highlights(&text, range.clone(), base, peak, delta);
            highlights.extend(rest.iter().cloned());
            StyledText::new(text.clone()).with_highlights(highlights)
        },
    )
}

/// A muted label that shimmers.
fn shimmering_label(
    selector: &'static str,
    label: impl Into<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    let label: SharedString = label.into();
    let len = label.len();
    div().debug_selector(move || selector.into()).child(shimmer(
        selector,
        label,
        0..len,
        Vec::new(),
        theme.muted_foreground,
        theme.foreground,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        CallDetails, MAX_BLOCK_LINES, SHIMMER_SWEEP, call_details, edit_preview, head,
        shimmer_highlights, split_read_note,
    };
    use gpui::{Hsla, hsla};
    use loom_client::proto::ToolCall;

    const BASE: Hsla = hsla(0., 0., 0.5, 1.);
    const PEAK: Hsla = hsla(0., 0., 0.1, 1.);

    /// The color of every character, by its byte offset.
    fn colors(text: &str, range: std::ops::Range<usize>, delta: f32) -> Vec<(usize, Hsla)> {
        let runs = shimmer_highlights(text, range.clone(), BASE, PEAK, delta);
        // The runs cover the range in order, on character boundaries.
        let mut end = range.start;
        for (run, _) in &runs {
            assert_eq!(run.start, end);
            assert!(text.is_char_boundary(run.end));
            end = run.end;
        }
        assert_eq!(end, range.end);
        text[range.clone()]
            .char_indices()
            .map(|(offset, _)| {
                let at = range.start + offset;
                let (_, style) = runs.iter().find(|(run, _)| run.contains(&at)).unwrap();
                (at, style.color.unwrap())
            })
            .collect()
    }

    #[test]
    fn the_shimmer_sweeps_a_band_across_the_text_and_then_rests() {
        let text = "Thinking…";
        let all = 0..text.len();
        // At the start of the loop, which reduced motion shows, and while
        // resting, the text has its own color.
        for delta in [0.0, SHIMMER_SWEEP + 0.05, 0.99] {
            assert!(
                colors(text, all.clone(), delta)
                    .iter()
                    .all(|(_, c)| *c == BASE)
            );
        }
        // The band moves from left to right and darkens toward the peak.
        let darkest = |delta| {
            colors(text, all.clone(), delta)
                .into_iter()
                .min_by(|a, b| a.1.l.total_cmp(&b.1.l))
                .unwrap()
        };
        let (early, early_color) = darkest(SHIMMER_SWEEP * 0.3);
        let (late, late_color) = darkest(SHIMMER_SWEEP * 0.7);
        assert!(early < late, "{early} then {late}");
        assert!(early_color.l < BASE.l && early_color.l > PEAK.l);
        assert!(late_color.l < BASE.l);
        // Only part of the text is in the band at once.
        let mid = colors(text, all, SHIMMER_SWEEP * 0.5);
        assert!(mid.iter().any(|(_, c)| *c == BASE));
    }

    #[test]
    fn the_shimmer_colors_only_its_range() {
        let text = "shell cargo test";
        let colors = colors(text, 0..5, SHIMMER_SWEEP * 0.5);
        assert_eq!(colors.len(), 5);
        assert!(colors.iter().any(|(_, c)| *c != BASE));
    }

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
    fn task_updates_show_their_changes() {
        let read = call("tasks_read", serde_json::json!({}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let update = call(
            "tasks_update",
            serde_json::json!({"changes": [
                {"action": "add", "text": "build\n it"},
                {"action": "add", "text": "test", "status": "in_progress"},
                {"action": "update", "id": 1, "status": "done"},
                {"action": "update", "id": "2", "text": "test more"},
                {"action": "remove", "id": 3},
            ]}),
        );
        assert_eq!(
            call_details(&update, false),
            code(
                "+ build it\n+ test (in_progress)\n#1 → done\n#2: test more\n- #3\n",
                "text"
            )
        );
    }

    #[test]
    fn notes_calls_show_their_text_or_a_diff() {
        let read = call("notes_read", serde_json::json!({}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let edit = call(
            "notes_edit",
            serde_json::json!({"edits": [{"old_text": "- [ ] a", "new_text": "- [x] a"}]}),
        );
        assert_eq!(
            call_details(&edit, false),
            CallDetails::Diff("-- [ ] a\n+- [x] a".into())
        );
        let write = call("notes_write", serde_json::json!({"text": "# Plan\n"}));
        assert_eq!(call_details(&write, false), code("# Plan\n", "text"));
        let long = "line\n".repeat(MAX_BLOCK_LINES + 3);
        let write = call("notes_write", serde_json::json!({"content": long}));
        let CallDetails::Code { text, .. } = call_details(&write, false) else {
            panic!("no code block");
        };
        assert!(text.ends_with("… 3 more lines not shown"), "{text}");
    }

    #[test]
    fn read_has_no_argument_block_and_unknown_tools_show_json() {
        let read = call("read", serde_json::json!({"path": "a.rs", "offset": 3}));
        assert_eq!(call_details(&read, false), CallDetails::None);
        let search = call(
            "search_web",
            serde_json::json!({"query": "x", "maxSources": 3}),
        );
        assert_eq!(call_details(&search, false), CallDetails::None);
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
        ContentPart, ReasoningEffort, SequencedEvent, SessionEvent, SessionId, SessionInfo,
        SessionState, ToolCall, ToolResult,
    };

    use super::{MessageList, gap_before, header_highlights, pane};
    use crate::conversation::{Conversation, Item, ToolItem, ToolStatus};

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

    /// The text of a prompt, a reply, its reasoning and a notice starts where
    /// the icons of tool calls and the reasoning toggle do.
    #[gpui::test]
    fn every_item_starts_its_content_at_the_same_edge(cx: &mut TestAppContext) {
        let (conversation, _, messages, cx) = open(0, cx);
        conversation.update(cx, |conversation, cx| {
            let call = ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
                risk: None,
            };
            for event in [
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text("a prompt")],
                },
                SessionEvent::ToolCallRequested { call },
                SessionEvent::ToolCallFinished {
                    call_id: "c1".into(),
                    result: ToolResult {
                        content: "fn main() {}".into(),
                        is_error: false,
                        details: None,
                    },
                },
                SessionEvent::AssistantMessage {
                    text: "a reply".into(),
                    reasoning: Some("some reasoning".into()),
                    model: "test/model".into(),
                },
                SessionEvent::ModelChanged {
                    model: "test/other".into(),
                    reasoning: Some(ReasoningEffort::High),
                },
            ] {
                apply(conversation, event);
            }
            cx.notify();
        });
        messages.update(cx, |messages, cx| {
            messages.show_reasoning.insert(2);
            cx.notify();
        });
        cx.run_until_parked();

        let mut left = |selector: &'static str| cx.debug_bounds(selector).unwrap().left();
        let edge = left("user-text-0");
        for selector in [
            "tool-icon-c1",
            "reasoning-icon-2",
            "reasoning-text-2",
            "assistant-text-2",
            "notice-3",
        ] {
            assert_eq!(left(selector), edge, "{selector}");
        }
        // The reasoning row and its text are as large as the reply: one line
        // of the text is as tall.
        let mut height = |selector: &'static str| cx.debug_bounds(selector).unwrap().size.height;
        let line = height("assistant-text-2");
        // The row is padded like a tool call's header.
        assert_eq!(height("reasoning-row-2"), line + px(8.));
        assert_eq!(height("reasoning-text-2"), line);
        // The reasoning sits in a card just like a tool call's.
        let mut card = |selector: &'static str| {
            let bounds = cx.debug_bounds(selector).unwrap();
            (bounds.left(), bounds.right())
        };
        assert_eq!(card("reasoning-card-2"), card("tool-card-c1"));
    }

    /// The border color painted around the element with `selector`.
    fn border_of(selector: &'static str, cx: &mut VisualTestContext) -> gpui::Hsla {
        let bounds = cx.debug_bounds(selector).expect(selector);
        cx.update(|window, _| {
            let bounds = bounds.scale(window.scale_factor());
            window
                .painted_quads()
                .into_iter()
                .find(|quad| quad.bounds == bounds && quad.border_widths.left.0 > 0.)
                .expect(selector)
                .border_color
        })
    }

    #[gpui::test]
    fn the_border_of_a_card_darkens_under_the_cursor(cx: &mut TestAppContext) {
        let (conversation, _, _, cx) = open(0, cx);
        let call = |id: &str| ToolCall {
            call_id: id.into(),
            name: "shell".into(),
            arguments: r#"{"command":"ls"}"#.into(),
            risk: None,
        };
        feed(
            &conversation,
            vec![
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text("a prompt")],
                },
                SessionEvent::ToolCallRequested { call: call("c1") },
                SessionEvent::ToolCallFinished {
                    call_id: "c1".into(),
                    result: ToolResult {
                        content: "a.rs".into(),
                        is_error: false,
                        details: None,
                    },
                },
                SessionEvent::AssistantMessage {
                    text: "a reply".into(),
                    reasoning: Some("some reasoning".into()),
                    model: "test/model".into(),
                },
                SessionEvent::ToolCallRequested { call: call("c2") },
                SessionEvent::ApprovalRequired {
                    call_id: "c2".into(),
                    reason: "exec".into(),
                },
            ],
            cx,
        );
        let (border, ring, warning) = cx.update(|_, cx| {
            let theme = gpui_component::ActiveTheme::theme(cx);
            (theme.border, theme.ring, theme.warning)
        });
        assert_ne!(border, ring);
        let cards = ["tool-card-c1", "reasoning-card-2"];
        let hover = |selector: &'static str, cx: &mut VisualTestContext| {
            let center = cx.debug_bounds(selector).unwrap().center();
            cx.simulate_mouse_move(center, None, Modifiers::default());
            cx.run_until_parked();
        };

        for card in cards {
            assert_eq!(border_of(card, cx), border, "{card}");
        }
        for card in cards {
            hover(card, cx);
            for other in cards {
                let expected = if other == card { ring } else { border };
                assert_eq!(
                    border_of(other, cx),
                    expected,
                    "{other} with {card} hovered"
                );
            }
        }
        // A call that needs approval keeps its warning border.
        hover("tool-card-c2", cx);
        assert_eq!(border_of("tool-card-c2", cx), warning);
        for card in cards {
            assert_eq!(border_of(card, cx), border, "{card}");
        }
    }

    #[test]
    fn tool_calls_sit_close_together_and_prompts_stand_apart() {
        let user = Item::User {
            text: "q".into(),
            images: Vec::new(),
        };
        let tool = Item::Tool(ToolItem {
            call: ToolCall {
                call_id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
                risk: None,
            },
            status: ToolStatus::Succeeded,
            approval_reason: None,
            decision: None,
            output: Vec::new(),
            output_truncated: false,
            result: None,
        });
        let reply = Item::Assistant {
            text: "a".into(),
            reasoning: String::new(),
            streaming: false,
        };
        assert_eq!(gap_before(None, Some(&user)), px(0.));
        assert_eq!(gap_before(Some(&tool), Some(&tool)), px(4.));
        assert_eq!(gap_before(Some(&tool), Some(&reply)), px(12.));
        assert_eq!(gap_before(Some(&reply), Some(&tool)), px(12.));
        assert_eq!(gap_before(Some(&tool), None), px(12.));
        assert_eq!(gap_before(Some(&reply), Some(&user)), px(24.));
        assert_eq!(gap_before(Some(&tool), Some(&user)), px(24.));
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
    fn live_output_starts_each_line_at_the_left(cx: &mut TestAppContext) {
        let (_, _, messages, cx) = open(0, cx);
        let mut tool = ToolItem {
            call: ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
                risk: None,
            },
            status: ToolStatus::Running,
            approval_reason: None,
            decision: None,
            output: b"one\ntwo\n".to_vec(),
            output_truncated: false,
            result: None,
        };
        let text = messages.update(cx, |messages, _| messages.live_text(&tool));
        assert_eq!(text, "one\ntwo");

        // More arrives: lines from a terminal, and a progress line that
        // rewrites itself.
        tool.output.extend_from_slice(b"three\r\nfour\n50%\r100%\n");
        let text = messages.update(cx, |messages, _| messages.live_text(&tool));
        assert_eq!(text, "one\ntwo\nthree\nfour\n100%");
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
        // Six items and the status row.
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

    /// Applies `events` to the conversation and draws the list again.
    /// What the list showed of the items before a clear does not carry over
    /// to the items that take their places.
    #[gpui::test]
    fn a_cleared_conversation_starts_the_list_over(cx: &mut TestAppContext) {
        let (conversation, _, _, cx) = open(0, cx);
        let turn = |prompt: &str| {
            vec![
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text(prompt)],
                },
                SessionEvent::AssistantMessage {
                    text: "a reply".into(),
                    reasoning: Some("some reasoning".into()),
                    model: "test/model".into(),
                },
            ]
        };
        feed(&conversation, turn("before"), cx);
        let row = cx.debug_bounds("reasoning-row-1").unwrap();
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(shown("reasoning-text-1", cx));

        feed(&conversation, vec![SessionEvent::Cleared], cx);
        assert!(!shown("user-text-0", cx));
        assert!(!shown("reasoning-card-1", cx));

        feed(&conversation, turn("after"), cx);
        assert!(shown("user-text-0", cx));
        assert!(shown("reasoning-card-1", cx));
        assert!(!shown("reasoning-text-1", cx));
    }

    fn feed(
        conversation: &Entity<Conversation>,
        events: Vec<SessionEvent>,
        cx: &mut VisualTestContext,
    ) {
        conversation.update(cx, |conversation, cx| {
            for event in events {
                apply(conversation, event);
            }
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn state(state: SessionState) -> SessionEvent {
        SessionEvent::StateChanged { state }
    }

    fn shown(selector: &'static str, cx: &mut VisualTestContext) -> bool {
        cx.debug_bounds(selector).is_some()
    }

    /// Whether the list keeps being drawn again when only time passes. A
    /// shimmer that just ended may still have one frame coming, which does
    /// not count.
    fn redraws(messages: &Entity<MessageList>, cx: &mut VisualTestContext) -> bool {
        let tick = |cx: &mut VisualTestContext| {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
        };
        tick(cx);
        take_rendered(messages, cx);
        for _ in 0..3 {
            tick(cx);
        }
        take_rendered(messages, cx) > 0
    }

    #[gpui::test]
    fn thinking_shows_once_and_the_reasoning_takes_its_place(cx: &mut TestAppContext) {
        let (conversation, _, messages, cx) = open(1, cx);
        feed(
            &conversation,
            vec![
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text("a prompt")],
                },
                state(SessionState::Thinking),
            ],
            cx,
        );
        let placeholder = cx.debug_bounds("thinking-icon").expect("no thinking row");
        assert!(shown("shimmer-thinking", cx));
        assert!(!shown("status-block", cx));
        // The shimmer draws the list again as time passes.
        assert!(redraws(&messages, cx));

        feed(
            &conversation,
            vec![SessionEvent::ReasoningDelta { text: "hm".into() }],
            cx,
        );
        let ix = conversation.read_with(cx, |c, _| c.items.len() - 1);
        let selector: &'static str = format!("reasoning-icon-{ix}").leak();
        // The reasoning row replaces the thinking row, in the same place.
        assert_eq!(cx.debug_bounds(selector), Some(placeholder));
        assert!(!shown("thinking-icon", cx));
        assert!(shown("shimmer-thinking", cx));

        feed(
            &conversation,
            vec![SessionEvent::AssistantDelta { text: "Hel".into() }],
            cx,
        );
        assert!(shown(selector, cx));
        assert!(!shown("shimmer-thinking", cx));
        assert!(!shown("status-block", cx));
        // Nothing shimmers, so nothing is drawn again until something changes.
        assert!(!redraws(&messages, cx));
    }

    #[gpui::test]
    fn a_tool_call_shows_that_it_runs_on_its_own_card(cx: &mut TestAppContext) {
        let (conversation, _, messages, cx) = open(1, cx);
        let call = |id: &str| ToolCall {
            call_id: id.into(),
            name: "shell".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
            risk: None,
        };
        feed(
            &conversation,
            vec![
                SessionEvent::UserMessage {
                    content: vec![ContentPart::text("a prompt")],
                },
                SessionEvent::ToolCallRequested { call: call("c1") },
                state(SessionState::RunningTool),
            ],
            cx,
        );
        // Queued, not running yet.
        assert!(!shown("shimmer-c1", cx));
        feed(
            &conversation,
            vec![SessionEvent::ToolCallStarted {
                call_id: "c1".into(),
            }],
            cx,
        );
        let header = cx
            .debug_bounds("shimmer-c1")
            .expect("the name does not shimmer");
        let icon = cx.debug_bounds("tool-icon-c1").unwrap();
        assert!(header.left() > icon.right(), "the header text shimmers");
        assert!(redraws(&messages, cx));
        assert!(!shown("status-block", cx));
        assert!(!shown("shimmer-thinking", cx));

        feed(
            &conversation,
            vec![
                SessionEvent::ToolCallFinished {
                    call_id: "c1".into(),
                    result: ToolResult {
                        content: String::new(),
                        is_error: false,
                        details: None,
                    },
                },
                SessionEvent::ToolCallRequested { call: call("c2") },
                SessionEvent::ApprovalRequired {
                    call_id: "c2".into(),
                    reason: "ask".into(),
                },
                state(SessionState::WaitingApproval),
            ],
            cx,
        );
        assert!(!shown("shimmer-c1", cx));
        // The card asks for the approval; nothing else does.
        assert!(!shown("shimmer-c2", cx));
        assert!(!shown("status-block", cx));
        assert!(!redraws(&messages, cx));
    }

    #[gpui::test]
    fn waiting_for_the_workspace_lock_has_its_own_status_block(cx: &mut TestAppContext) {
        let (conversation, _, _, cx) = open(1, cx);
        feed(&conversation, vec![state(SessionState::WaitingLock)], cx);
        assert!(shown("status-block", cx));
        assert!(shown("shimmer-lock", cx));
        feed(&conversation, vec![state(SessionState::Idle)], cx);
        assert!(!shown("status-block", cx));
        assert!(!shown("shimmer-lock", cx));
    }

    #[gpui::test]
    fn the_header_colors_the_name_the_timeout_and_the_arguments_apart(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let call = ToolCall {
            call_id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({ "command": "cargo test", "timeout": 30 }).to_string(),
            risk: None,
        };
        let summary = crate::conversation::tool_summary(&call);
        for mode in [
            gpui_component::ThemeMode::Light,
            gpui_component::ThemeMode::Dark,
        ] {
            cx.update(|cx| {
                gpui_component::Theme::change(mode, None, cx);
                let theme = gpui_component::ActiveTheme::theme(cx);
                let spans = header_highlights(&call, &summary, theme);
                let parts: Vec<_> = spans
                    .iter()
                    .map(|(range, style)| (&summary[range.clone()], style.color.unwrap()))
                    .collect();
                assert_eq!(parts[0].0, "shell");
                assert_eq!(parts[1].0, " (timeout: 30s)");
                assert_eq!(parts[2].0, " cargo test");
                // Each part differs from the others and from the reply text.
                let mut colors: Vec<_> = parts.iter().map(|(_, color)| *color).collect();
                colors.push(theme.foreground);
                for (i, a) in colors.iter().enumerate() {
                    for b in &colors[i + 1..] {
                        assert_ne!(a, b, "{mode:?}: {parts:?}");
                    }
                }
            });
        }
    }
}
