use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use loom_client::proto::{PullRequestResult, RepositoryStatus};
use loom_client::{CloseChoice, close_plan, needs_choice};
use loom_view_model::codespaces::work_text;

pub type OnClose = Rc<dyn Fn(Vec<String>, Option<String>, &mut Window, &mut App)>;

pub struct CloseCodespaceView {
    title: String,
    statuses: Vec<RepositoryStatus>,
    choices: HashMap<String, CloseChoice>,
    message: Entity<InputState>,
    error: Option<String>,
    on_close: OnClose,
}

impl CloseCodespaceView {
    fn choose(&mut self, directory: String, choice: CloseChoice, cx: &mut Context<Self>) {
        self.choices.insert(directory, choice);
        self.error = None;
        cx.notify();
    }

    fn needs_message(&self) -> bool {
        self.statuses.iter().any(|s| {
            s.changed_files > 0 && self.choices.get(&s.directory) == Some(&CloseChoice::Push)
        })
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.message.read(cx).value().trim().to_owned();
        let message = (!message.is_empty()).then_some(message);
        let choices = self.choices.clone();
        match close_plan(
            &self.statuses,
            |directory| choices.get(directory).copied(),
            message.is_some(),
        ) {
            Ok(push) => {
                window.close_dialog(cx);
                let message = if push.is_empty() { None } else { message };
                (self.on_close)(push, message, window, cx);
            }
            Err(err) => {
                self.error = Some(format!("{}.", capitalize(&err.to_string())));
                cx.notify();
            }
        }
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl Render for CloseCodespaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let with_work: Vec<RepositoryStatus> = self
            .statuses
            .iter()
            .filter(|s| needs_choice(s))
            .cloned()
            .collect();
        let description = if with_work.is_empty() {
            format!(
                "Close the codespace {}. No repository has unsaved work. The oracle removes the container and its data.",
                self.title
            )
        } else {
            format!(
                "Close the codespace {}. The oracle removes the container and its data. For each repository with unsaved work, push the branch or discard the work.",
                self.title
            )
        };
        let needs_message = self.needs_message();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(description),
            )
            .children(with_work.into_iter().enumerate().map(|(ix, status)| {
                let chosen = self.choices.get(&status.directory).copied();
                let push_dir = status.directory.clone();
                let discard_dir = status.directory.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(status.directory.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if status.error.is_some() {
                                        theme.danger
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .whitespace_normal()
                                    .child(format!("{} · {}", status.branch, work_text(&status))),
                            ),
                    )
                    .child(
                        Button::new(("close-push", ix))
                            .small()
                            .label("Push")
                            .disabled(status.error.is_some())
                            .when(chosen == Some(CloseChoice::Push), |b| b.primary())
                            .when(chosen != Some(CloseChoice::Push), |b| b.outline())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choose(push_dir.clone(), CloseChoice::Push, cx)
                            })),
                    )
                    .child(
                        Button::new(("close-discard", ix))
                            .small()
                            .label("Discard")
                            .when(chosen == Some(CloseChoice::Discard), |b| b.danger())
                            .when(chosen != Some(CloseChoice::Discard), |b| b.outline())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choose(discard_dir.clone(), CloseChoice::Discard, cx)
                            })),
                    )
            }))
            .when(needs_message, |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child("Commit message"))
                        .child(Input::new(&self.message)),
                )
            })
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .whitespace_normal()
                        .child(error),
                )
            })
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("close-codespace")
                        .danger()
                        .label("Close codespace")
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
            )
    }
}

pub fn open_close(
    title: String,
    statuses: Vec<RepositoryStatus>,
    on_close: OnClose,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| CloseCodespaceView {
        title,
        statuses,
        choices: HashMap::new(),
        message: cx
            .new(|cx| InputState::new(window, cx).placeholder("Save the work of the codespace")),
        error: None,
        on_close,
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Close codespace")
            .w(px(560.))
            .child(view.clone())
    });
}

pub type OnOpen = Rc<dyn Fn(String, String, bool, &mut Window, &mut App)>;

pub struct PullRequestView {
    statuses: Vec<RepositoryStatus>,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    draft: bool,
    error: Option<String>,
    on_open: OnOpen,
}

impl PullRequestView {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().trim().to_owned();
        if title.is_empty() {
            self.error = Some("Type a title.".to_owned());
            cx.notify();
            return;
        }
        let body = self.body.read(cx).value().to_string();
        window.close_dialog(cx);
        (self.on_open)(title, body, self.draft, window, cx);
    }
}

impl Render for PullRequestView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child("The agent pushes the branch of each repository with commits and opens a pull request to its base branch. A repository that has a pull request gets the new title and body."),
            )
            .children(self.statuses.iter().map(|status| {
                let base = status
                    .base_branch
                    .clone()
                    .unwrap_or_else(|| "the default branch".to_owned());
                let detail = match &status.pull_request {
                    Some(url) => format!("{} → {base} · {url}", status.branch),
                    None => format!(
                        "{} → {base} · {} commit{}",
                        status.branch,
                        status.ahead,
                        if status.ahead == 1 { "" } else { "s" }
                    ),
                };
                v_flex()
                    .child(div().text_sm().child(status.directory.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(detail),
                    )
            }))
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child("Title"))
                    .child(Input::new(&self.title)),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child("Body"))
                    .child(Textarea::new(&self.body)),
            )
            .child(
                Checkbox::new("pull-request-draft")
                    .label("Draft")
                    .checked(self.draft)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.draft = *checked;
                        cx.notify();
                    })),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .whitespace_normal()
                        .child(error),
                )
            })
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("open-pull-request")
                        .primary()
                        .label("Open pull request")
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
            )
    }
}

pub fn open_pull_request(
    statuses: Vec<RepositoryStatus>,
    title: String,
    on_open: OnOpen,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| PullRequestView {
        statuses,
        title: cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Pull request title");
            state.set_value(title, window, cx);
            state
        }),
        body: cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 12)
                .placeholder("Description (optional)")
        }),
        draft: false,
        error: None,
        on_open,
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Open pull request")
            .w(px(560.))
            .child(view.clone())
    });
}

pub fn pull_request_summary(results: &[PullRequestResult]) -> (Vec<String>, Vec<SharedString>) {
    let mut lines = Vec::new();
    let mut urls = Vec::new();
    for result in results {
        let text = match (&result.url, &result.skipped, &result.error) {
            (_, _, Some(error)) => format!("{}: error: {error}", result.directory),
            (Some(url), _, None) if result.created => {
                urls.push(SharedString::from(url.clone()));
                format!("{}: opened {url}", result.directory)
            }
            (Some(url), _, None) => {
                urls.push(SharedString::from(url.clone()));
                format!("{}: updated {url}", result.directory)
            }
            (None, Some(reason), None) => format!("{}: skipped: {reason}", result.directory),
            (None, None, None) => format!("{}: nothing to do", result.directory),
        };
        lines.push(text);
    }
    (lines, urls)
}
