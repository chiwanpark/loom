//! The Tasks tab of the right panel: a session's task list, the plan the
//! agent keeps for the session with its tasks tools. The user changes the
//! same list here, and each change applies at once.

use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Escape, InputEvent, Textarea, TextareaState};
use gpui_component::{ActiveTheme, Disableable, Icon, Sizable, h_flex, v_flex};
use loom_client::proto::{SessionId, Task, TaskChange, TaskStatus, TasksContent};

use crate::state::{AgentConnection, ConnectionEvent};
use crate::ui::notify;

pub(crate) const CONTEXT: &str = "TasksView";

/// A task whose text is being edited.
struct Editing {
    id: u32,
    input: Entity<TextareaState>,
    _subscription: Subscription,
}

pub struct TasksView {
    connection: Entity<AgentConnection>,
    session: SessionId,
    /// `None` until the list is first read.
    tasks: Option<Vec<Task>>,
    max_tasks: u32,
    /// Why the list could not be read the last time.
    error: Option<String>,
    /// Counts the change notices, so an answer that left before the latest
    /// notice, and may hold an older list, does not replace that notice's.
    notices: u64,
    loading: bool,
    /// Whether to read again once the current read is answered.
    load_again: bool,
    new_task: Entity<TextareaState>,
    editing: Option<Editing>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl TasksView {
    pub fn new(
        connection: Entity<AgentConnection>,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let new_task = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder("Add a task (Enter to add, Shift+Enter for a new line)")
                .submit_on_enter(true);
            state.set_auto_grow(1, 6, cx);
            state
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &new_task,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { shift: false, .. } = event {
                        this.add(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&connection, window, |this, _, event, window, cx| {
                match event {
                    ConnectionEvent::TasksChanged { session, tasks }
                        if *session == this.session =>
                    {
                        this.notices += 1;
                        this.show(tasks.clone(), cx);
                    }
                    // Changes made while the link was down sent no notice.
                    ConnectionEvent::SessionsLoaded => this.load(window, cx),
                    _ => {}
                }
            }),
        ];
        let mut this = Self {
            connection,
            session,
            tasks: None,
            max_tasks: 0,
            error: None,
            notices: 0,
            loading: false,
            load_again: false,
            new_task,
            editing: None,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.load(window, cx);
        this
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The tasks as the view shows them; `None` until they are first read.
    pub fn tasks(&self) -> Option<&[Task]> {
        self.tasks.as_deref()
    }

    pub fn input_focus(&self, cx: &App) -> FocusHandle {
        self.new_task.read(cx).focus_handle(cx)
    }

    fn show(&mut self, tasks: Vec<Task>, cx: &mut Context<Self>) {
        if let Some(editing) = &self.editing
            && !tasks.iter().any(|task| task.id == editing.id)
        {
            self.editing = None;
        }
        self.tasks = Some(tasks);
        self.error = None;
        cx.notify();
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            self.load_again = true;
            return;
        }
        self.loading = true;
        let notices = self.notices;
        let task = self.connection.read(cx).read_tasks(self.session.clone());
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Some(Ok(content)) => this.answered(content, notices, cx),
                    Some(Err(err)) => {
                        this.error = Some(err.to_string());
                        cx.notify();
                    }
                    None => {}
                }
                if std::mem::take(&mut this.load_again) {
                    this.load(window, cx);
                }
            });
        })
        .detach();
    }

    /// Takes an answer sent after `notices` notices. Every change sends a
    /// notice, in order, so when one came since, the latest notice's list is
    /// at least as new as the answer.
    fn answered(&mut self, content: TasksContent, notices: u64, cx: &mut Context<Self>) {
        self.max_tasks = content.max_tasks;
        if notices == self.notices || self.tasks.is_none() {
            self.show(content.tasks, cx);
        } else {
            cx.notify();
        }
    }

    /// Sends `changes` to the agent, which applies them all or none.
    fn update(&mut self, changes: Vec<TaskChange>, window: &mut Window, cx: &mut Context<Self>) {
        let notices = self.notices;
        let task = self
            .connection
            .read(cx)
            .update_tasks(self.session.clone(), changes);
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Some(Ok(content)) => this.answered(content, notices, cx),
                Some(Err(err)) => {
                    notify::error(window, cx, format!("Could not change the tasks: {err}"));
                    // The list may have moved on in a way this view missed.
                    this.load(window, cx);
                }
                None => {}
            });
        })
        .detach();
    }

    fn is_full(&self) -> bool {
        self.tasks
            .as_ref()
            .is_some_and(|tasks| tasks.len() >= self.max_tasks as usize)
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.new_task.read(cx).value().trim().to_owned();
        if text.is_empty() || self.tasks.is_none() {
            return;
        }
        if self.is_full() {
            notify::error(
                window,
                cx,
                format!("The task list holds at most {} tasks.", self.max_tasks),
            );
            return;
        }
        self.new_task
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.update(
            vec![TaskChange::Add {
                text,
                status: TaskStatus::Created,
            }],
            window,
            cx,
        );
    }

    /// Moves a task on to its next status: created, in progress, done, and
    /// created again.
    fn cycle(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task(id) else {
            return;
        };
        let status = task.status.next();
        self.update(
            vec![TaskChange::Update {
                id,
                text: None,
                status: Some(status),
            }],
            window,
            cx,
        );
    }

    fn remove(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.as_ref().is_some_and(|e| e.id == id) {
            self.editing = None;
        }
        self.update(vec![TaskChange::Remove { id }], window, cx);
    }

    /// Removes the tasks that are done.
    fn clear_done(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes: Vec<TaskChange> = self
            .tasks
            .iter()
            .flatten()
            .filter(|task| task.status == TaskStatus::Done)
            .map(|task| TaskChange::Remove { id: task.id })
            .collect();
        if !changes.is_empty() {
            self.update(changes, window, cx);
        }
    }

    fn task(&self, id: u32) -> Option<&Task> {
        self.tasks.as_ref()?.iter().find(|task| task.id == id)
    }

    /// Edits a task's text in place of the text.
    fn start_edit(&mut self, id: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task(id) else {
            return;
        };
        let text = task.text.clone();
        let input = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).submit_on_enter(true);
            state.set_auto_grow(1, 8, cx);
            state.set_value(text, window, cx);
            state
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { shift: false, .. } | InputEvent::Blur => {
                    this.finish_edit(window, cx)
                }
                _ => {}
            },
        );
        input.update(cx, |input, cx| input.focus(window, cx));
        self.editing = Some(Editing {
            id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Saves the edited text, unless it is empty or the same as before.
    fn finish_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let text = editing.input.read(cx).value().trim().to_owned();
        if !text.is_empty() && self.task(editing.id).is_some_and(|task| task.text != text) {
            self.update(
                vec![TaskChange::Update {
                    id: editing.id,
                    text: Some(text),
                    status: None,
                }],
                window,
                cx,
            );
        }
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    /// A task's row; `width` is the width of the widest task number, so that
    /// the texts line up.
    fn render_task(
        &self,
        task: &Task,
        width: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let id = task.id;
        let (icon, color, tooltip) = match task.status {
            TaskStatus::Created => (
                gpui_kit_assets::IconName::Circle,
                theme.muted_foreground,
                "Not started: click to start",
            ),
            TaskStatus::InProgress => (
                gpui_kit_assets::IconName::CircleDot,
                theme.primary,
                "In progress: click to mark done",
            ),
            TaskStatus::Done => (
                gpui_kit_assets::IconName::CircleCheck,
                theme.success,
                "Done: click to start over",
            ),
        };
        let done = task.status == TaskStatus::Done;
        let editing = self
            .editing
            .as_ref()
            .filter(|editing| editing.id == id)
            .map(|editing| editing.input.clone());
        let text_color = if done {
            theme.muted_foreground
        } else {
            theme.foreground
        };
        h_flex()
            .id(SharedString::from(format!("task-{id}")))
            .w_full()
            .items_start()
            .px_2()
            .py_0p5()
            .gap_1()
            .hover(|el| el.bg(theme.list_hover))
            // The buttons are as tall as a line of text, and padded like the
            // text, so that they line up with the first line of a long task.
            .child(
                div().flex_none().py_0p5().child(
                    Button::new(SharedString::from(format!("task-status-{id}")))
                        .debug_selector(move || format!("task-status-{id}"))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(icon).text_color(color))
                        .tooltip(tooltip)
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.cycle(id, window, cx)),
                        ),
                ),
            )
            // The number the agent calls the task by, as in "#3".
            .child(
                div()
                    .debug_selector(move || format!("task-number-{id}"))
                    .flex_none()
                    .py_0p5()
                    .font_family(theme.mono_font_family.clone())
                    .text_sm()
                    .line_height(LINE_HEIGHT)
                    .text_color(theme.muted_foreground)
                    .whitespace_nowrap()
                    .child(task_number(id, width)),
            )
            .child(match editing {
                Some(input) => div()
                    .flex_1()
                    .min_w_0()
                    .on_action(cx.listener(|this, _: &Escape, _, cx| this.cancel_edit(cx)))
                    .child(Textarea::new(&input).small())
                    .into_any_element(),
                None => div()
                    .id(SharedString::from(format!("task-text-{id}")))
                    .debug_selector(move || format!("task-text-{id}"))
                    .flex_1()
                    .min_w_0()
                    .py_0p5()
                    .text_sm()
                    .line_height(LINE_HEIGHT)
                    .text_color(text_color)
                    .when(done, |el| el.line_through())
                    .cursor_text()
                    .child(task.text.clone())
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.start_edit(id, window, cx)),
                    )
                    .into_any_element(),
            })
            .child(
                div().flex_none().py_0p5().child(
                    Button::new(SharedString::from(format!("task-remove-{id}")))
                        .debug_selector(move || format!("task-remove-{id}"))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(gpui_kit_assets::IconName::Trash))
                        .tooltip("Remove the task")
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.remove(id, window, cx)),
                        ),
                ),
            )
    }
}

impl Focusable for TasksView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TasksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tasks = self.tasks.clone().unwrap_or_default();
        let done = tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Done)
            .count();
        let detail = match (&self.tasks, tasks.len()) {
            (None, _) => String::new(),
            (Some(_), 0) => "No tasks".to_owned(),
            (Some(_), n) => format!("{done} of {n} done"),
        };
        let width = number_width(&tasks);
        let rows: Vec<_> = tasks
            .iter()
            .map(|task| self.render_task(task, width, cx))
            .collect();
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .id("tasks-summary")
                            .debug_selector(|| "tasks-summary".into())
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                    .child(
                        Button::new("tasks-clear-done")
                            .debug_selector(|| "tasks-clear-done".into())
                            .ghost()
                            .xsmall()
                            .label("Clear done")
                            .disabled(done == 0)
                            .tooltip("Remove the tasks that are done, for the agent too")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.clear_done(window, cx)),
                            ),
                    ),
            )
            .map(|el| match (&self.tasks, &self.error) {
                (None, Some(error)) => el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(format!("Could not read the tasks: {error}")),
                ),
                (None, None) => el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Loading the tasks…"),
                ),
                (Some(_), _) => el
                    .child(
                        v_flex()
                            .id("tasks-list")
                            .flex_1()
                            .min_h_0()
                            .py_1()
                            .overflow_y_scroll()
                            .when(tasks.is_empty(), |el| {
                                el.child(
                                    div()
                                        .px_3()
                                        .py_2()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(
                                            "The agent plans its work here, and you can add \
                                             tasks too. Click a task's circle to change its \
                                             status, and its text to edit it.",
                                        ),
                                )
                            })
                            .children(rows),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex_none()
                            .p_2()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                Textarea::new(&self.new_task)
                                    .small()
                                    .disabled(self.is_full()),
                            ),
                    ),
            })
    }
}

/// The line height of a task's text: the height of an extra-small button,
/// so that the buttons beside a task line up with its first line.
const LINE_HEIGHT: gpui::Rems = gpui::Rems(1.25);

/// A task's number as the agent writes it, `#3`, right-aligned in `width`
/// characters.
fn task_number(id: u32, width: usize) -> String {
    format!("{:>width$}", format!("#{id}"))
}

/// The width of the widest task number in `tasks`.
fn number_width(tasks: &[Task]) -> usize {
    tasks
        .iter()
        .map(|task| task.id.to_string().len() + 1)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::Root;

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    /// A tasks view on a new session, and a second connection that stands in
    /// for another client, or the agent, changing the list.
    fn open<'a>(
        agent: &TestAgent,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<TasksView>,
        Entity<AgentConnection>,
        &'a mut VisualTestContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let conn = agent.connect(cx);
        let other = agent.connect(cx);
        let client = conn.read_with(cx, |conn, _| conn.client.clone());
        let session = futures::executor::block_on(
            agent
                .backend
                .run(async move { client.create_session(None).await }),
        )
        .unwrap()
        .unwrap()
        .id;
        let slot = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let slot = slot.clone();
            move |window, cx| {
                let view = cx.new(|cx| TasksView::new(conn, session, window, cx));
                *slot.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let view = slot.borrow_mut().take().unwrap();
        wait_until(cx, |cx| view.read(cx).tasks.is_some());
        (view, other, cx)
    }

    /// The list on the agent, read over the other connection.
    fn agent_tasks(
        view: &Entity<TasksView>,
        other: &Entity<AgentConnection>,
        cx: &mut VisualTestContext,
    ) -> Vec<(u32, String, TaskStatus)> {
        let session = view.read_with(cx, |view, _| view.session.clone());
        let task = other.read_with(cx, |conn, _| conn.read_tasks(session));
        let content = futures::executor::block_on(task).unwrap().unwrap();
        content
            .tasks
            .into_iter()
            .map(|t| (t.id, t.text, t.status))
            .collect()
    }

    fn shown(
        view: &Entity<TasksView>,
        cx: &mut VisualTestContext,
    ) -> Vec<(u32, String, TaskStatus)> {
        view.read_with(cx, |view, _| {
            view.tasks
                .iter()
                .flatten()
                .map(|t| (t.id, t.text.clone(), t.status))
                .collect()
        })
    }

    fn change_elsewhere(
        view: &Entity<TasksView>,
        other: &Entity<AgentConnection>,
        changes: Vec<TaskChange>,
        cx: &mut VisualTestContext,
    ) {
        let session = view.read_with(cx, |view, _| view.session.clone());
        let task = other.read_with(cx, |conn, _| conn.update_tasks(session, changes));
        futures::executor::block_on(task).unwrap().unwrap();
    }

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no {selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn tasks_are_added_with_enter(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        assert!(view.read_with(cx, |view, _| view.max_tasks) > 0);
        view.update_in(cx, |view, window, cx| {
            view.input_focus(cx).focus(window, cx)
        });
        cx.simulate_input("write the plan");
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 1));
        let expected = vec![(1, "write the plan".to_owned(), TaskStatus::Created)];
        assert_eq!(agent_tasks(&view, &other, cx), expected);
        assert_eq!(shown(&view, cx), expected);
        // The input is ready for the next one.
        assert_eq!(
            view.read_with(cx, |view, cx| view.new_task.read(cx).value().to_string()),
            ""
        );
        // Blank text adds nothing.
        cx.simulate_input("   ");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(agent_tasks(&view, &other, cx).len(), 1);
    }

    #[test]
    fn task_numbers_are_written_as_the_agent_writes_them() {
        let task = |id| Task {
            id,
            text: "t".into(),
            status: TaskStatus::Created,
        };
        assert_eq!(number_width(&[]), 0);
        assert_eq!(number_width(&[task(3)]), 2);
        assert_eq!(number_width(&[task(3), task(12)]), 3);
        assert_eq!(task_number(3, 2), "#3");
        assert_eq!(task_number(3, 3), " #3");
        assert_eq!(task_number(12, 3), "#12");
    }

    /// Each task shows the number the agent calls it by, which stays when
    /// earlier tasks go, and the texts line up whatever the numbers' widths.
    #[gpui::test]
    fn tasks_show_their_numbers(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        let add = |text: String| TaskChange::Add {
            text,
            status: TaskStatus::Created,
        };
        change_elsewhere(
            &view,
            &other,
            (1..=10).map(|n| add(format!("t{n}"))).collect(),
            cx,
        );
        change_elsewhere(
            &view,
            &other,
            (2..=9).map(|id| TaskChange::Remove { id }).collect(),
            cx,
        );
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 2));
        cx.run_until_parked();

        assert!(cx.debug_bounds("task-number-2").is_none());
        let mut bounds = |selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("no {selector}"))
        };
        let (one, ten) = (bounds("task-number-1"), bounds("task-number-10"));
        let (text_one, text_ten) = (bounds("task-text-1"), bounds("task-text-10"));
        let status = bounds("task-status-1");
        let remove = bounds("task-remove-1");
        // The numbers sit between the status and the text, padded to one width.
        assert!(status.right() <= one.left());
        assert!(one.right() <= text_one.left());
        assert_eq!(one.size.width, ten.size.width);
        assert_eq!(text_one.left(), text_ten.left());
        // The buttons line up with the first line of the text.
        let middle = |b: gpui::Bounds<gpui::Pixels>| b.top() + b.size.height / 2.;
        assert_eq!(middle(status), middle(one));
        assert_eq!(middle(remove), middle(one));
        assert_eq!(one.top(), text_one.top());
    }

    /// A task that wraps keeps its buttons by its first line.
    #[gpui::test]
    fn buttons_line_up_with_the_first_line_of_a_long_task(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        change_elsewhere(
            &view,
            &other,
            vec![TaskChange::Add {
                text: "a long task ".repeat(40),
                status: TaskStatus::Done,
            }],
            cx,
        );
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 1));
        cx.run_until_parked();
        let mut bounds = |selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("no {selector}"))
        };
        let (status, number, text) = (
            bounds("task-status-1"),
            bounds("task-number-1"),
            bounds("task-text-1"),
        );
        assert!(text.size.height > number.size.height * 2., "{text:?}");
        let middle = |b: gpui::Bounds<gpui::Pixels>| b.top() + b.size.height / 2.;
        assert_eq!(middle(status), middle(number));
    }

    #[gpui::test]
    fn clicking_the_status_cycles_it(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        change_elsewhere(
            &view,
            &other,
            vec![TaskChange::Add {
                text: "a".into(),
                status: TaskStatus::Created,
            }],
            cx,
        );
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 1));
        for status in [
            TaskStatus::InProgress,
            TaskStatus::Done,
            TaskStatus::Created,
        ] {
            click("task-status-1", cx);
            wait_until(cx, |cx| {
                view.read(cx).tasks().is_some_and(|t| t[0].status == status)
            });
            assert_eq!(agent_tasks(&view, &other, cx)[0].2, status);
        }
    }

    #[gpui::test]
    fn tasks_are_edited_in_place_and_removed(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        change_elsewhere(
            &view,
            &other,
            vec![
                TaskChange::Add {
                    text: "a".into(),
                    status: TaskStatus::Created,
                },
                TaskChange::Add {
                    text: "b".into(),
                    status: TaskStatus::Done,
                },
            ],
            cx,
        );
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 2));

        click("task-text-1", cx);
        assert!(view.read_with(cx, |view, _| view.editing.is_some()));
        cx.simulate_keystrokes("end");
        cx.simulate_input(" more");
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| {
            view.read(cx).tasks().is_some_and(|t| t[0].text == "a more")
        });
        assert!(view.read_with(cx, |view, _| view.editing.is_none()));

        // Escape leaves the text as it was.
        click("task-text-1", cx);
        cx.simulate_input("x");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.editing.is_none()));
        assert_eq!(agent_tasks(&view, &other, cx)[0].1, "a more");

        click("task-remove-1", cx);
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 1));
        assert_eq!(
            agent_tasks(&view, &other, cx),
            [(2, "b".to_owned(), TaskStatus::Done)]
        );
        click("tasks-clear-done", cx);
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.is_empty()));
        assert!(agent_tasks(&view, &other, cx).is_empty());
    }

    #[gpui::test]
    fn changes_elsewhere_show_up_and_a_reconnect_reads_again(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        change_elsewhere(
            &view,
            &other,
            vec![TaskChange::Add {
                text: "theirs".into(),
                status: TaskStatus::InProgress,
            }],
            cx,
        );
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 1));
        // Forget the list, as if the notice never came.
        view.update(cx, |view, _| view.tasks = Some(Vec::new()));
        let conn = view.read_with(cx, |view, _| view.connection.clone());
        conn.update(cx, |_, cx| cx.emit(ConnectionEvent::SessionsLoaded));
        wait_until(cx, |cx| view.read(cx).tasks().is_some_and(|t| t.len() == 1));
        assert_eq!(
            shown(&view, cx),
            [(1, "theirs".to_owned(), TaskStatus::InProgress)]
        );
    }

    #[gpui::test]
    fn an_older_answer_does_not_replace_a_newer_notice(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, _other, cx) = open(&agent, cx);
        let newer = vec![Task {
            id: 2,
            text: "newer".into(),
            status: TaskStatus::Created,
        }];
        view.update(cx, |view, cx| {
            let sent_at = view.notices;
            view.notices += 1;
            view.show(newer.clone(), cx);
            view.answered(
                TasksContent {
                    tasks: Vec::new(),
                    max_tasks: 5,
                },
                sent_at,
                cx,
            );
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.tasks.clone()),
            Some(newer)
        );
        assert_eq!(view.read_with(cx, |view, _| view.max_tasks), 5);
    }
}
