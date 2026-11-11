use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ListAlignment, ListHorizontalSizingBehavior, ListOffset,
    ListState, ParentElement, Render, ScrollStrategy, SharedString, Styled, Subscription,
    UniformListScrollHandle, Window, div, list, prelude::FluentBuilder, px, uniform_list,
};
use gpui_component::scroll::{ScrollableElement, ScrollbarAxis, ScrollbarHandle};
use gpui_component::{ActiveTheme, h_flex, v_flex};
use unicode_width::UnicodeWidthChar;

use crate::changes::{
    DiffLine, DiffLineKind, change_lines, count_lines, diff_lines, session_changes,
};
use crate::conversation::Conversation;
use crate::layout::SavedTab;
use crate::state::AgentConnection;
use crate::ui::file_view::wrap_button;
use crate::ui::workspace::ToggleLineWrap;

const ROW_HEIGHT: f32 = 20.;
/// How far past the visible rows the wrapping list lays out, so rows don't
/// pop in while scrolling.
const OVERDRAW: f32 = 400.;
/// Spaces a tab is shown as.
const TAB_WIDTH: usize = 4;

enum Source {
    Session {
        conversation: Entity<Conversation>,
        revision: Option<u64>,
        _subscription: Subscription,
    },
    Git {
        connection: Entity<AgentConnection>,
        relative: String,
        old_path: Option<String>,
        diff: Option<String>,
        error: Option<String>,
        truncated: bool,
        loading: bool,
    },
}

pub struct DiffView {
    source: Source,
    path: PathBuf,
    title: String,
    lines: Rc<Vec<DiffLine>>,
    added: usize,
    removed: usize,
    focus: FocusHandle,
    /// Whether long lines wrap. Unwrapped lines are shown by `scroll`, which
    /// needs rows of one height and scrolls sideways; wrapped ones by `list`,
    /// which measures each row.
    wrap: bool,
    scroll: UniformListScrollHandle,
    list: ListState,
    /// The line that takes the most columns, which sets how far unwrapped
    /// lines scroll sideways.
    widest: usize,
}

impl DiffView {
    pub fn session(
        conversation: Entity<Conversation>,
        path: PathBuf,
        title: String,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&conversation, |this, _, cx| {
            if this.refresh_session(cx) {
                cx.notify();
            }
        });
        let mut this = Self {
            source: Source::Session {
                conversation,
                revision: None,
                _subscription: subscription,
            },
            path,
            title,
            lines: Rc::new(Vec::new()),
            added: 0,
            removed: 0,
            focus: cx.focus_handle(),
            wrap,
            scroll: UniformListScrollHandle::new(),
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            widest: 0,
        };
        this.refresh_session(cx);
        this
    }

    pub fn git(
        connection: Entity<AgentConnection>,
        relative: String,
        old_path: Option<String>,
        wrap: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = connection.read(cx).info.workspace.join(&relative);
        let title = match &old_path {
            Some(old) => format!("{old} → {relative}"),
            None => relative.clone(),
        };
        let mut this = Self {
            source: Source::Git {
                connection,
                relative,
                old_path,
                diff: None,
                error: None,
                truncated: false,
                loading: false,
            },
            path,
            title,
            lines: Rc::new(Vec::new()),
            added: 0,
            removed: 0,
            focus: cx.focus_handle(),
            wrap,
            scroll: UniformListScrollHandle::new(),
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            widest: 0,
        };
        this.reload(cx);
        this
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// How to open this view again after a restart.
    pub fn saved_tab(&self) -> SavedTab {
        match &self.source {
            Source::Session { .. } => SavedTab::SessionDiff {
                path: self.path.clone(),
            },
            Source::Git {
                relative, old_path, ..
            } => SavedTab::GitDiff {
                path: relative.clone(),
                old_path: old_path.clone(),
            },
        }
    }

    pub fn is_git(&self) -> bool {
        matches!(self.source, Source::Git { .. })
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Source::Git {
            connection,
            relative,
            old_path,
            loading,
            ..
        } = &mut self.source
        else {
            return;
        };
        if *loading {
            return;
        }
        *loading = true;
        let task = connection
            .read(cx)
            .file_diff(relative.clone(), old_path.clone());
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(result) = result {
                    this.apply_git(result, cx);
                } else if let Source::Git { loading, .. } = &mut this.source {
                    *loading = false;
                }
            });
        })
        .detach();
    }

    fn apply_git(
        &mut self,
        result: loom_client::Result<loom_client::proto::FileDiff>,
        cx: &mut Context<Self>,
    ) {
        let Source::Git {
            diff,
            error,
            truncated,
            loading,
            ..
        } = &mut self.source
        else {
            return;
        };
        *loading = false;
        match result {
            Ok(fetched) => {
                let changed = diff.as_deref() != Some(fetched.diff.as_str())
                    || *truncated != fetched.truncated
                    || error.is_some();
                if !changed {
                    return;
                }
                *error = None;
                *truncated = fetched.truncated;
                let (added, removed) = count_lines(&fetched.diff);
                let mut lines = diff_lines(&fetched.diff);
                if fetched.truncated {
                    lines.push(DiffLine {
                        kind: DiffLineKind::Note,
                        old: None,
                        new: None,
                        text: "The diff is too large and was truncated.".into(),
                    });
                }
                *diff = Some(fetched.diff);
                self.added = added;
                self.removed = removed;
                self.set_lines(lines);
            }
            Err(err) => {
                let message = err.to_string();
                if error.as_ref() == Some(&message) {
                    return;
                }
                *error = Some(message);
            }
        }
        cx.notify();
    }

    /// Recomputes the diff after the session's changes did; returns whether
    /// they did.
    fn refresh_session(&mut self, cx: &mut Context<Self>) -> bool {
        let Source::Session {
            conversation,
            revision,
            ..
        } = &mut self.source
        else {
            return false;
        };
        let conversation = conversation.read(cx);
        if *revision == Some(conversation.changes_revision) {
            return false;
        }
        *revision = Some(conversation.changes_revision);
        let change = session_changes(&conversation.items)
            .into_iter()
            .find(|c| c.path == self.path);
        match change {
            Some(change) => {
                self.added = change.added;
                self.removed = change.removed;
                self.set_lines(change_lines(&change));
            }
            None => {
                self.added = 0;
                self.removed = 0;
                self.set_lines(Vec::new());
            }
        }
        true
    }

    fn set_lines(&mut self, lines: Vec<DiffLine>) {
        self.widest = widest_line(&lines);
        // Resetting the wrapping list forgets where it was scrolled to; the
        // unwrapped one keeps its offset by itself.
        let top = self.list.logical_scroll_top();
        self.list
            .reset_with_uniform_height(lines.len(), px(ROW_HEIGHT));
        self.list.scroll_to(top);
        self.lines = Rc::new(lines);
    }

    /// Switches between wrapped and unwrapped lines, keeping the top line in
    /// view.
    pub fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        if self.wrap {
            let top = self.list.logical_scroll_top().item_ix;
            self.scroll.scroll_to_item_strict(top, ScrollStrategy::Top);
        } else {
            let offset = -ScrollbarHandle::offset(&self.scroll).y;
            let top = (offset / px(ROW_HEIGHT)).floor().max(0.) as usize;
            self.list.scroll_to(ListOffset {
                item_ix: top,
                offset_in_item: px(0.),
            });
        }
        self.wrap = !self.wrap;
        cx.notify();
    }

    fn empty_message(&self) -> Option<String> {
        if !self.lines.is_empty() {
            return None;
        }
        Some(match &self.source {
            Source::Session { .. } => "This session has not changed this file.".into(),
            Source::Git {
                error: Some(error), ..
            } => format!("Could not load the diff: {error}"),
            Source::Git { diff: None, .. } => "Loading…".into(),
            Source::Git { .. } => "This file has no uncommitted changes.".into(),
        })
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
        let empty = self.empty_message();
        let body = div()
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .font_family(theme.mono_font_family.clone())
            .text_sm();
        let body = if self.wrap {
            body.child(
                list(self.list.clone(), move |ix, _, _| match lines.get(ix) {
                    Some(line) => render_line(line.clone(), true, &row_theme),
                    None => div().into_any_element(),
                })
                .size_full(),
            )
            .vertical_scrollbar(&self.list)
        } else {
            body.child(
                uniform_list("diff-lines", count, move |range, _, _| {
                    range
                        .filter_map(|ix| lines.get(ix).cloned())
                        .map(|line| render_line(line, false, &row_theme))
                        .collect::<Vec<_>>()
                })
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(Some(self.widest))
                .size_full()
                .track_scroll(&self.scroll),
            )
            .scrollbar(&self.scroll, ScrollbarAxis::Both)
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ToggleLineWrap, _, cx| this.toggle_wrap(cx)))
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
                    .when(self.is_git(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("against HEAD"),
                        )
                    })
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
                    )
                    .child(
                        wrap_button("diff-wrap", self.wrap)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_wrap(cx))),
                    ),
            )
            .when_some(empty, |el, message| {
                el.child(
                    div()
                        .p_4()
                        .text_color(theme.muted_foreground)
                        .child(message),
                )
            })
            .child(body)
    }
}

/// Columns `text` takes in a monospace font, with tabs shown as spaces and
/// wide characters such as Hangul taking two.
fn columns(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '\t' => TAB_WIDTH,
            c => c.width().unwrap_or(0),
        })
        .sum()
}

/// The index of the line that takes the most columns, or 0 for no lines.
fn widest_line(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .enumerate()
        .max_by_key(|(ix, line)| (columns(&line.text), std::cmp::Reverse(*ix)))
        .map_or(0, |(ix, _)| ix)
}

/// Renders a diff line. Unwrapped rows are one line tall and as wide as their
/// text; wrapped rows fill the width and grow to fit the text.
fn render_line(line: DiffLine, wrap: bool, theme: &gpui_component::Theme) -> AnyElement {
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
    let text = line.text.replace('\t', &" ".repeat(TAB_WIDTH));
    h_flex()
        .w_full()
        .map(|row| {
            if wrap {
                row.items_start()
                    .min_h(px(ROW_HEIGHT))
                    .line_height(px(ROW_HEIGHT))
            } else {
                row.h(px(ROW_HEIGHT)).whitespace_nowrap()
            }
        })
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
                .map(|text| {
                    if wrap {
                        text.flex_1().min_w_0()
                    } else {
                        text.flex_none()
                    }
                })
                .when_some(color, |el, c| el.text_color(c))
                .child(text),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Context,
            old: None,
            new: None,
            text: text.into(),
        }
    }

    #[test]
    fn counts_columns_like_a_monospace_font() {
        assert_eq!(columns("abc"), 3);
        assert_eq!(columns("\tx"), TAB_WIDTH + 1);
        // Hangul takes two columns per character.
        assert_eq!(columns("한글"), 4);
    }

    #[test]
    fn picks_the_line_taking_the_most_columns() {
        assert_eq!(widest_line(&[]), 0);
        let lines = [line("abcde"), line("한글한"), line("\tab"), line("abcdef")];
        // "한글한" has fewer characters than "abcde" but takes six columns, and
        // the tab line takes six too; the first of the widest wins.
        assert_eq!(widest_line(&lines), 1);
        // Two tabs take eight columns, more than seven letters.
        let lines = [line("abc"), line("\t\t"), line("abcdefg")];
        assert_eq!(widest_line(&lines), 1);
    }
}

#[cfg(test)]
mod view_tests {
    use std::path::PathBuf;

    use gpui::{AppContext, Entity, ScrollStrategy, TestAppContext, VisualTestContext, px};
    use gpui_component::scroll::ScrollbarHandle;
    use loom_client::proto::{SessionId, SessionInfo, SessionState};

    use super::{DiffLine, DiffLineKind, DiffView, ROW_HEIGHT};
    use crate::conversation::Conversation;

    const LINES: usize = 200;
    /// Lines 3 and 4 are far wider than any window: one of words, one a single
    /// token as in minified code.
    const SPACED: usize = 3;
    const UNSPACED: usize = 4;

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

    fn lines() -> Vec<DiffLine> {
        (0..LINES)
            .map(|ix| DiffLine {
                kind: DiffLineKind::Context,
                old: Some(ix as u32 + 1),
                new: Some(ix as u32 + 1),
                text: match ix {
                    SPACED => "word ".repeat(600),
                    UNSPACED => "x".repeat(3000),
                    _ => format!("line {ix}"),
                },
            })
            .collect()
    }

    fn open(wrap: bool, cx: &mut TestAppContext) -> (Entity<DiffView>, &mut VisualTestContext) {
        cx.update(gpui_component::init);
        let conversation = cx.new(|_| Conversation::new(info()));
        let (view, cx) = cx.add_window_view(|_, cx| {
            let mut view = DiffView::session(
                conversation,
                PathBuf::from("/w/a.rs"),
                "a.rs".into(),
                wrap,
                cx,
            );
            view.set_lines(lines());
            view
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn redraw(view: &Entity<DiffView>, cx: &mut VisualTestContext) {
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn unwrapped_lines_scroll_both_ways_with_scrollbars(cx: &mut TestAppContext) {
        let (view, cx) = open(false, cx);
        assert!(cx.debug_bounds("scrollbar-overlay").is_some());
        view.read_with(cx, |view, _| {
            let viewport = view.scroll.viewport_bounds().size;
            let content = ScrollbarHandle::content_size(&view.scroll);
            assert!(viewport.width > px(0.) && viewport.height > px(0.));
            assert_eq!(content.height, px(ROW_HEIGHT) * LINES as f32);
            // The widest line, not the first, sets how far rows scroll.
            assert!(
                content.width > viewport.width * 2.,
                "content {content:?} in viewport {viewport:?}"
            );
        });
    }

    #[gpui::test]
    fn wrapped_lines_grow_instead_of_scrolling_sideways(cx: &mut TestAppContext) {
        let (view, cx) = open(true, cx);
        assert!(cx.debug_bounds("scrollbar-overlay").is_some());
        view.read_with(cx, |view, _| {
            let height = |ix| view.list.bounds_for_item(ix).unwrap().size.height;
            assert_eq!(height(0), px(ROW_HEIGHT));
            assert!(height(SPACED) > px(ROW_HEIGHT) * 2., "{:?}", height(SPACED));
            assert!(
                height(UNSPACED) > px(ROW_HEIGHT) * 2.,
                "{:?}",
                height(UNSPACED)
            );
            let viewport = ScrollbarHandle::viewport_bounds(&view.list).size;
            let content = ScrollbarHandle::content_size(&view.list);
            assert!(
                content.width <= viewport.width,
                "{content:?} in {viewport:?}"
            );
            assert!(
                content.height > viewport.height,
                "{content:?} in {viewport:?}"
            );
        });
    }

    #[gpui::test]
    fn toggling_wrap_keeps_the_top_line(cx: &mut TestAppContext) {
        let (view, cx) = open(false, cx);
        view.update(cx, |view, _| {
            view.scroll.scroll_to_item_strict(120, ScrollStrategy::Top)
        });
        redraw(&view, cx);
        view.update(cx, |view, cx| view.toggle_wrap(cx));
        redraw(&view, cx);
        view.read_with(cx, |view, _| {
            assert!(view.wrap);
            assert_eq!(view.list.logical_scroll_top().item_ix, 120);
        });
        // Scrolled while wrapped, the unwrapped rows follow rather than going
        // back to where they were.
        view.update(cx, |view, _| {
            view.list.scroll_to(gpui::ListOffset {
                item_ix: 60,
                offset_in_item: px(0.),
            })
        });
        redraw(&view, cx);
        view.update(cx, |view, cx| view.toggle_wrap(cx));
        redraw(&view, cx);
        view.read_with(cx, |view, _| {
            assert!(!view.wrap);
            let offset = ScrollbarHandle::offset(&view.scroll);
            assert_eq!(offset.y, px(-ROW_HEIGHT * 60.));
        });
    }

    #[gpui::test]
    fn new_lines_keep_the_wrapped_list_where_it_was(cx: &mut TestAppContext) {
        let (view, cx) = open(true, cx);
        view.update(cx, |view, _| {
            view.list.scroll_to(gpui::ListOffset {
                item_ix: 120,
                offset_in_item: px(0.),
            })
        });
        redraw(&view, cx);
        view.update(cx, |view, cx| {
            view.set_lines(lines());
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.list.logical_scroll_top().item_ix, 120)
        });
    }
}
