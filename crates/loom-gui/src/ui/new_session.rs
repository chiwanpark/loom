//! The dialog that starts a session on a host: in the workspace of one of
//! its agents, or in a new directory, for which it starts an agent.

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Sizable, WindowExt, h_flex, v_flex};

/// The workspace of an agent that the host runs.
#[derive(Debug, Clone)]
pub struct WorkspaceChoice {
    pub label: String,
    /// How the client is linked to the agent, when it is not connected.
    pub detail: Option<String>,
    pub instance: String,
}

/// Where the user chose to start the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// In the workspace of a running agent, in a new worktree with
    /// `worktree`.
    Agent { instance: String, worktree: bool },
    /// In a directory, for which a new agent starts.
    Directory(PathBuf),
}

pub type OnPick = Rc<dyn Fn(Pick, &mut Window, &mut App)>;

pub struct NewSessionView {
    host: String,
    choices: Vec<WorkspaceChoice>,
    path: Entity<InputState>,
    on_pick: OnPick,
    _subscription: Subscription,
}

impl NewSessionView {
    #[cfg(test)]
    pub fn choices(&self) -> &[WorkspaceChoice] {
        &self.choices
    }

    fn pick(&mut self, pick: Pick, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        (self.on_pick)(pick, window, cx);
    }

    fn start_in_directory(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.read(cx).value().trim().to_owned();
        if path.is_empty() {
            return;
        }
        self.pick(Pick::Directory(PathBuf::from(path)), window, cx);
    }
}

impl Render for NewSessionView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let heading = |text: &'static str| div().text_sm().child(text);
        let choices = self.choices.iter().enumerate().map(|(ix, choice)| {
            let open = choice.instance.clone();
            let worktree = choice.instance.clone();
            h_flex()
                .id(("workspace-choice", ix))
                .debug_selector(move || format!("workspace-choice-{ix}"))
                .w_full()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let pick = Pick::Agent {
                        instance: open.clone(),
                        worktree: false,
                    };
                    this.pick(pick, window, cx)
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .child(choice.label.clone()),
                        )
                        .when_some(choice.detail.clone(), |el, detail| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(detail),
                            )
                        }),
                )
                .child(
                    // The row must not see the button's clicks.
                    div()
                        .debug_selector(move || format!("worktree-choice-{ix}"))
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new(("worktree-choice", ix))
                                .ghost()
                                .xsmall()
                                .label("In a worktree…")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let pick = Pick::Agent {
                                        instance: worktree.clone(),
                                        worktree: true,
                                    };
                                    this.pick(pick, window, cx)
                                })),
                        ),
                )
        });
        let has_choices = !self.choices.is_empty();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(format!(
                        "Start a session on {}. Choose the workspace of a running agent, or enter a directory to start an agent for it. The directory is on the agent host.",
                        self.host
                    )),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(heading("Running agents"))
                    .when(has_choices, |el| {
                        el.child(
                            v_flex()
                                .id("workspace-choices")
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .children(choices),
                        )
                    })
                    .when(!has_choices, |el| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from(format!(
                                    "No agents run on {}.",
                                    self.host
                                ))),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(heading("New workspace directory"))
                    .child(Input::new(&self.path)),
            )
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("start-in-directory")
                        .primary()
                        .label("Start")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.start_in_directory(window, cx)
                        })),
                ),
            )
    }
}

/// Opens the dialog for `host`, which lists the workspaces of its agents.
pub fn open(
    host: String,
    choices: Vec<WorkspaceChoice>,
    on_pick: OnPick,
    window: &mut Window,
    cx: &mut App,
) -> Entity<NewSessionView> {
    let view = cx.new(|cx| {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("/home/me/project"));
        let subscription = cx.subscribe_in(
            &path,
            window,
            |this: &mut NewSessionView, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.start_in_directory(window, cx);
                }
            },
        );
        NewSessionView {
            host,
            choices,
            path,
            on_pick,
            _subscription: subscription,
        }
    });
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("New session")
            .w(px(520.))
            .child(dialog_view.clone())
    });
    let input = view.read(cx).path.clone();
    window.defer(cx, move |window, cx| {
        input.update(cx, |input, cx| input.focus(window, cx));
    });
    view
}
