use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AsyncApp, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder, px, uniform_list,
};
use gpui_component::{ActiveTheme, h_flex, v_flex};

use crate::changes::{
    DiffLine, DiffLineKind, change_lines, count_lines, diff_lines, session_changes,
};
use crate::conversation::Conversation;
use crate::state::AgentConnection;

const ROW_HEIGHT: f32 = 20.;

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
    scroll: UniformListScrollHandle,
}

impl DiffView {
    pub fn session(
        conversation: Entity<Conversation>,
        path: PathBuf,
        title: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&conversation, |this, _, cx| {
            this.refresh_session(cx);
            cx.notify();
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
            scroll: UniformListScrollHandle::new(),
        };
        this.refresh_session(cx);
        this
    }

    pub fn git(
        connection: Entity<AgentConnection>,
        relative: String,
        old_path: Option<String>,
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
            scroll: UniformListScrollHandle::new(),
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
                self.lines = Rc::new(lines);
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

    fn refresh_session(&mut self, cx: &mut Context<Self>) {
        let Source::Session {
            conversation,
            revision,
            ..
        } = &mut self.source
        else {
            return;
        };
        let conversation = conversation.read(cx);
        if *revision == Some(conversation.revision) {
            return;
        }
        *revision = Some(conversation.revision);
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
