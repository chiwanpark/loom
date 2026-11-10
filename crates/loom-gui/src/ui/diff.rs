use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder, px, uniform_list,
};
use gpui_component::{ActiveTheme, h_flex, v_flex};

use crate::changes::{DiffLine, DiffLineKind, change_lines, session_changes};
use crate::conversation::Conversation;

const ROW_HEIGHT: f32 = 20.;

pub struct DiffView {
    conversation: Entity<Conversation>,
    path: PathBuf,
    title: String,
    lines: Rc<Vec<DiffLine>>,
    added: usize,
    removed: usize,
    revision: Option<u64>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

impl DiffView {
    pub fn new(
        conversation: Entity<Conversation>,
        path: PathBuf,
        title: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&conversation, |this, _, cx| {
            this.refresh(cx);
            cx.notify();
        });
        let mut this = Self {
            conversation,
            path,
            title,
            lines: Rc::new(Vec::new()),
            added: 0,
            removed: 0,
            revision: None,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
        };
        this.refresh(cx);
        this
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let conversation = self.conversation.read(cx);
        if self.revision == Some(conversation.revision) {
            return;
        }
        self.revision = Some(conversation.revision);
        let change = session_changes(&conversation.items)
            .into_iter()
            .find(|c| c.path == self.path);
        match change {
            Some(change) => {
                self.added = change.added;
                self.removed = change.removed;
                self.lines = Rc::new(change_lines(&change));
            }
            None => {
                self.added = 0;
                self.removed = 0;
                self.lines = Rc::new(Vec::new());
            }
        }
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn line_number(number: Option<u32>) -> SharedString {
    number
        .map(|n| SharedString::from(n.to_string()))
        .unwrap_or_default()
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lines = self.lines.clone();
        let count = lines.len();
        let row_theme = theme.clone();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.green)
                            .child(format!("+{}", self.added)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.red)
                            .child(format!("−{}", self.removed)),
                    ),
            )
            .when(count == 0, |el| {
                el.child(
                    div()
                        .p_4()
                        .text_color(theme.muted_foreground)
                        .child("This session has not changed this file."),
                )
            })
            .child(
                uniform_list("diff-lines", count, move |range, _, _| {
                    range
                        .filter_map(|ix| lines.get(ix).cloned())
                        .map(|line| render_line(line, &row_theme))
                        .collect::<Vec<_>>()
                })
                .flex_1()
                .w_full()
                .font_family(theme.mono_font_family.clone())
                .text_sm()
                .track_scroll(&self.scroll),
            )
    }
}

fn render_line(line: DiffLine, theme: &gpui_component::Theme) -> AnyElement {
    let (marker, background, color) = match line.kind {
        DiffLineKind::Added => ("+", Some(theme.green.opacity(0.15)), None),
        DiffLineKind::Removed => ("-", Some(theme.red.opacity(0.15)), None),
        DiffLineKind::Hunk => ("", Some(theme.muted), Some(theme.cyan)),
        DiffLineKind::Note | DiffLineKind::File => {
            ("", Some(theme.muted), Some(theme.muted_foreground))
        }
        DiffLineKind::Context => (" ", None, None),
    };
    let gutter = |number: Option<u32>| {
        div()
            .w(px(48.))
            .flex_none()
            .pr_2()
            .text_right()
            .text_color(theme.muted_foreground)
            .child(line_number(number))
    };
    h_flex()
        .h(px(ROW_HEIGHT))
        .w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .when_some(background, |el, bg| el.bg(bg))
        .child(gutter(line.old))
        .child(gutter(line.new))
        .child(
            div()
                .w(px(16.))
                .flex_none()
                .text_color(theme.muted_foreground)
                .child(marker),
        )
        .child(
            div()
                .flex_1()
                .when_some(color, |el, c| el.text_color(c))
                .child(line.text.replace('\t', "    ")),
        )
        .into_any_element()
}
