//! The Thread tab of the right panel: a session's operation thread, the
//! notes the agent keeps for the session with its thread tools. The user
//! reads and edits the same text here.

use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Subscription, Task, Window,
    div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Editor, EditorState, InputEvent};
use gpui_component::{ActiveTheme, Disableable, Sizable, h_flex, v_flex};
use loom_client::proto::{ErrorCode, FileVersion, SessionId, ThreadContent};

use crate::state::{AgentConnection, ConnectionEvent};
use crate::ui::file_view::human_size;
use crate::ui::workspace::Save;
use crate::ui::{dialogs, notify};

pub(crate) const CONTEXT: &str = "ThreadView";

/// The thread as the agent holds it, as far as this view knows: the text
/// it last read or saved, and that text's version.
struct Saved {
    text: String,
    version: FileVersion,
}

pub struct ThreadView {
    connection: Entity<AgentConnection>,
    session: SessionId,
    editor: Entity<EditorState>,
    /// `None` until the thread is first read.
    saved: Option<Saved>,
    max_bytes: u64,
    /// Why the thread could not be read the last time.
    error: Option<String>,
    /// The thread's version on the agent, when it moved on from `saved`
    /// while the editor had changes that are not saved.
    remote: Option<FileVersion>,
    /// Whether the editor's text differs from `saved`.
    dirty: bool,
    loading: bool,
    /// A read to make once the current one is answered, since the thread
    /// may have changed after the agent answered it; `Some(true)` throws
    /// away the editor's changes.
    load_again: Option<bool>,
    saving: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl ThreadView {
    pub fn new(
        connection: Entity<AgentConnection>,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("text")
                .line_number(false)
                .searchable(true)
                .soft_wrap(true)
                .placeholder("Notes the agent keeps for this session")
        });
        let subscriptions = vec![
            cx.subscribe(&editor, |this: &mut Self, _, event, cx| {
                if let InputEvent::Change = event {
                    this.update_dirty(cx);
                }
            }),
            cx.subscribe_in(&connection, window, |this, _, event, window, cx| {
                match event {
                    ConnectionEvent::ThreadChanged { session, version }
                        if *session == this.session
                            && this.saved.as_ref().is_none_or(|s| s.version != *version) =>
                    {
                        this.load(false, window, cx)
                    }
                    // Changes made while the link was down sent no notice.
                    ConnectionEvent::SessionsLoaded => this.load(false, window, cx),
                    _ => {}
                }
            }),
        ];
        let mut this = Self {
            connection,
            session,
            editor,
            saved: None,
            max_bytes: 0,
            error: None,
            remote: None,
            dirty: false,
            loading: false,
            load_again: None,
            saving: false,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.load(false, window, cx);
        this
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn connection(&self) -> &Entity<AgentConnection> {
        &self.connection
    }

    /// Whether the editor has changes that are not saved.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The most bytes the thread may hold; 0 until it is first read.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn is_saving(&self) -> bool {
        self.saving
    }

    pub fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    fn update_dirty(&mut self, cx: &mut Context<Self>) {
        let dirty = self
            .saved
            .as_ref()
            .is_some_and(|saved| *self.editor.read(cx).text() != saved.text.as_str());
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.notify();
        }
    }

    /// Reads the thread. The text replaces the editor's when the editor has
    /// no changes by the time it arrives, or when `discard` says to throw
    /// them away; otherwise the view only learns that the thread moved on.
    fn load(&mut self, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            self.load_again = Some(discard || self.load_again == Some(true));
            return;
        }
        self.loading = true;
        let task = self.connection.read(cx).read_thread(self.session.clone());
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Some(Ok(content)) => this.loaded(content, discard, window, cx),
                    Some(Err(err)) => this.error = Some(err.to_string()),
                    None => {}
                }
                if let Some(discard) = this.load_again.take() {
                    this.load(discard, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn loaded(
        &mut self,
        content: ThreadContent,
        discard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        self.max_bytes = content.max_bytes;
        let same = *self.editor.read(cx).text() == content.text.as_str();
        if self.dirty && !discard && !same {
            let moved = self
                .saved
                .as_ref()
                .is_some_and(|saved| saved.version != content.version);
            self.remote = moved.then_some(content.version);
            return;
        }
        if !same {
            self.editor.update(cx, |editor, cx| {
                editor.set_value(content.text.clone(), window, cx)
            });
        }
        self.saved = Some(Saved {
            text: content.text,
            version: content.version,
        });
        self.remote = None;
        self.dirty = false;
    }

    /// Saves the editor's text over the thread the view last read. The task
    /// says whether the thread holds the text now. A thread that changed on
    /// the agent since is not overwritten; the edits stay in the editor.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        let Some(saved) = &self.saved else {
            return Task::ready(false);
        };
        if !self.dirty {
            return Task::ready(true);
        }
        let expected = saved.version;
        self.write(expected, window, cx)
    }

    /// Saves the editor's text over the thread's newer text on the agent.
    fn overwrite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(remote) = self.remote {
            self.write(remote, window, cx).detach();
        }
    }

    fn write(
        &mut self,
        expected: FileVersion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        if self.saving {
            return Task::ready(false);
        }
        let text = self.text(cx);
        if text.len() as u64 > self.max_bytes {
            notify::error(
                window,
                cx,
                format!(
                    "The thread was not saved: it holds at most {}.",
                    human_size(self.max_bytes)
                ),
            );
            return Task::ready(false);
        }
        self.saving = true;
        cx.notify();
        let task =
            self.connection
                .read(cx)
                .write_thread(self.session.clone(), text.clone(), expected);
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                cx.notify();
                match result {
                    Some(Ok(version)) => {
                        this.saved = Some(Saved { text, version });
                        this.remote = None;
                        this.update_dirty(cx);
                        true
                    }
                    Some(Err(err)) => {
                        let message = match err.api() {
                            Some(api) if api.code == ErrorCode::Conflict => {
                                // Learn the new version, for the notice and
                                // for overwriting.
                                this.load(false, window, cx);
                                "The thread was not saved: it changed since it was \
                                 loaded. Reload it, or overwrite it with your text."
                                    .to_string()
                            }
                            _ => format!("Could not save the thread: {err}"),
                        };
                        notify::error(window, cx, message);
                        false
                    }
                    None => false,
                }
            })
            .unwrap_or(false)
        })
    }

    fn save_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(window, cx).detach();
    }

    /// Throws away the editor's changes and reads the thread again, after
    /// asking.
    fn revert_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty {
            self.load(true, window, cx);
            return;
        }
        let this = cx.entity().downgrade();
        dialogs::confirm(
            "Discard your changes?",
            "Reloading the thread throws away the changes you have not saved.",
            "Discard and reload",
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.load(true, window, cx));
            },
            window,
            cx,
        );
    }

    /// Empties the thread, for the agent too, after asking.
    fn clear_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut description =
            "The agent's notes for this session are deleted. The conversation stays.".to_string();
        if self.dirty {
            description.push_str(" Your changes that are not saved are thrown away too.");
        }
        let this = cx.entity().downgrade();
        dialogs::confirm(
            "Clear the thread?",
            description,
            "Clear thread",
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.clear(window, cx));
            },
            window,
            cx,
        );
    }

    /// Empties the editor and saves that. When the thread changed on the
    /// agent meanwhile, nothing is lost there: the view offers to reload
    /// or overwrite, as for any save.
    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = &self.saved else {
            return;
        };
        let expected = saved.version;
        self.editor
            .update(cx, |editor, cx| editor.set_value("", window, cx));
        self.update_dirty(cx);
        if self.dirty {
            self.write(expected, window, cx).detach();
        }
    }
}

impl Focusable for ThreadView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThreadView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let loaded = self.saved.is_some();
        let size = self.editor.read(cx).text().len() as u64;
        let too_large = loaded && size > self.max_bytes;
        let empty = size == 0 && self.saved.as_ref().is_none_or(|s| s.text.is_empty());
        let detail = if loaded {
            let mut detail = format!("{} of {}", human_size(size), human_size(self.max_bytes));
            if self.saving {
                detail.push_str(" · saving…");
            }
            detail
        } else {
            String::new()
        };
        let stale = self.remote.is_some() && !self.saving;
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save_clicked(window, cx)))
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
                            .id("thread-size")
                            .debug_selector(|| "thread-size".into())
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(if too_large {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .when(self.dirty, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .child(if self.dirty {
                                format!("{detail} · not saved")
                            } else {
                                detail
                            }),
                    )
                    .child(
                        Button::new("thread-save")
                            .debug_selector(|| "thread-save".into())
                            .ghost()
                            .xsmall()
                            .label("Save")
                            .disabled(!self.dirty || self.saving || too_large)
                            .tooltip_with_action("Save the thread", &Save, Some(CONTEXT))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_clicked(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("thread-revert")
                            .debug_selector(|| "thread-revert".into())
                            .ghost()
                            .xsmall()
                            .label("Revert")
                            .disabled(!self.dirty || self.saving)
                            .tooltip("Throw away your changes and reload the thread")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.revert_clicked(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("thread-clear")
                            .debug_selector(|| "thread-clear".into())
                            .ghost()
                            .xsmall()
                            .label("Clear thread")
                            .disabled(!loaded || empty || self.saving)
                            .tooltip("Empty the thread, for the agent too")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.clear_clicked(window, cx)),
                            ),
                    ),
            )
            .when(stale, |el| {
                el.child(
                    h_flex()
                        .id("thread-stale")
                        .debug_selector(|| "thread-stale".into())
                        .w_full()
                        .flex_none()
                        .px_2()
                        .py_1()
                        .gap_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(theme.warning.opacity(0.15))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .child("The thread changed elsewhere while you were editing it."),
                        )
                        .child(
                            Button::new("thread-reload")
                                .debug_selector(|| "thread-reload".into())
                                .ghost()
                                .xsmall()
                                .label("Reload")
                                .tooltip("Throw away your changes and show the new text")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.revert_clicked(window, cx)
                                })),
                        )
                        .child(
                            Button::new("thread-overwrite")
                                .debug_selector(|| "thread-overwrite".into())
                                .ghost()
                                .xsmall()
                                .label("Overwrite")
                                .disabled(too_large)
                                .tooltip("Replace the new text with yours")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.overwrite(window, cx)),
                                ),
                        ),
                )
            })
            .map(|el| match (&self.saved, &self.error) {
                (None, Some(error)) => el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(format!("Could not read the thread: {error}")),
                ),
                (None, None) => el.child(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Loading the thread…"),
                ),
                (Some(_), _) => el.child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&self.editor)
                            .bordered(false)
                            .appearance(false)
                            .h_full()
                            .small(),
                    ),
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::{Root, WindowExt};

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    /// A thread view on a new session, with its editor focused, and a second
    /// connection that stands in for another client changing the thread.
    fn open<'a>(
        agent: &TestAgent,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<ThreadView>,
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
                let view = cx.new(|cx| ThreadView::new(conn, session, window, cx));
                *slot.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let view = slot.borrow_mut().take().unwrap();
        wait_until(cx, |cx| view.read(cx).saved.is_some());
        view.update_in(cx, |view, window, cx| {
            view.editor_focus(cx).focus(window, cx)
        });
        cx.run_until_parked();
        (view, other, cx)
    }

    fn text(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> String {
        view.read_with(cx, |view, cx| view.text(cx))
    }

    /// The thread on the agent, read over the other connection.
    fn thread(
        view: &Entity<ThreadView>,
        other: &Entity<AgentConnection>,
        cx: &mut VisualTestContext,
    ) -> ThreadContent {
        let session = view.read_with(cx, |view, _| view.session.clone());
        let task = other.read_with(cx, |conn, _| conn.read_thread(session));
        futures::executor::block_on(task).unwrap().unwrap()
    }

    /// Replaces the thread from the other connection.
    fn write_elsewhere(
        view: &Entity<ThreadView>,
        other: &Entity<AgentConnection>,
        text: &str,
        cx: &mut VisualTestContext,
    ) {
        let current = thread(view, other, cx).version;
        let session = view.read_with(cx, |view, _| view.session.clone());
        let task = other.read_with(cx, |conn, _| {
            conn.write_thread(session, text.into(), current)
        });
        futures::executor::block_on(task).unwrap().unwrap();
    }

    fn save(view: &Entity<ThreadView>, cx: &mut VisualTestContext) -> bool {
        let saved = view.update_in(cx, |view, window, cx| view.save(window, cx));
        wait_until(cx, |cx| !view.read(cx).saving);
        futures::executor::block_on(saved)
    }

    fn has_dialog(cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| window.has_active_dialog(cx))
    }

    #[gpui::test]
    fn edits_are_saved_with_ctrl_s(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        assert_eq!(text(&view, cx), "");
        assert!(view.read_with(cx, |view, _| view.max_bytes) > 0);

        cx.simulate_input("plan");
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        cx.simulate_keystrokes("ctrl-s");
        wait_until(cx, |cx| !view.read(cx).is_dirty());
        assert_eq!(thread(&view, &other, cx).text, "plan");
        // A second save builds on the first.
        cx.simulate_input(" two");
        assert!(save(&view, cx));
        assert_eq!(thread(&view, &other, cx).text, "plan two");
    }

    #[gpui::test]
    fn a_change_elsewhere_shows_up_when_there_are_no_edits(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        write_elsewhere(&view, &other, "from the agent", cx);
        wait_until(cx, |cx| view.read(cx).text(cx) == "from the agent");
        assert!(!view.read_with(cx, |view, _| view.is_dirty()));
        assert!(cx.debug_bounds("thread-stale").is_none());
        // The view saves over the new text, not the one it first read.
        cx.simulate_input("x");
        assert!(save(&view, cx));
        assert_eq!(thread(&view, &other, cx).text, "xfrom the agent");
    }

    #[gpui::test]
    fn a_change_elsewhere_keeps_unsaved_edits_and_offers_to_overwrite(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        cx.simulate_input("mine");
        write_elsewhere(&view, &other, "theirs", cx);
        wait_until(cx, |cx| view.read(cx).remote.is_some());
        assert_eq!(text(&view, cx), "mine");
        assert!(cx.debug_bounds("thread-stale").is_some());

        // Saving does not overwrite the other change.
        assert!(!save(&view, cx));
        assert_eq!(thread(&view, &other, cx).text, "theirs");
        assert_eq!(text(&view, cx), "mine");

        // Overwriting does, on purpose.
        view.update_in(cx, |view, window, cx| view.overwrite(window, cx));
        wait_until(cx, |cx| !view.read(cx).saving);
        assert_eq!(thread(&view, &other, cx).text, "mine");
        assert!(!view.read_with(cx, |view, _| view.is_dirty()));
        assert!(cx.debug_bounds("thread-stale").is_none());
    }

    #[gpui::test]
    fn a_failed_save_learns_of_the_change_and_reload_takes_it(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        cx.simulate_input("mine");
        write_elsewhere(&view, &other, "theirs", cx);
        wait_until(cx, |cx| view.read(cx).remote.is_some());
        // Forget the change, as when the notice of it was missed.
        view.update(cx, |view, _| view.remote = None);

        assert!(!save(&view, cx));
        wait_until(cx, |cx| view.read(cx).remote.is_some());
        assert_eq!(text(&view, cx), "mine");

        // Reloading asks before throwing the edits away.
        view.update_in(cx, |view, window, cx| view.revert_clicked(window, cx));
        assert!(has_dialog(cx));
        assert_eq!(text(&view, cx), "mine");
        cx.update(|window, cx| window.close_dialog(cx));
        view.update_in(cx, |view, window, cx| view.load(true, window, cx));
        wait_until(cx, |cx| view.read(cx).text(cx) == "theirs");
        assert!(!view.read_with(cx, |view, _| view.is_dirty()));
        assert!(view.read_with(cx, |view, _| view.remote.is_none()));
    }

    #[gpui::test]
    fn a_reconnect_reads_the_thread_again(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        write_elsewhere(&view, &other, "theirs", cx);
        wait_until(cx, |cx| view.read(cx).text(cx) == "theirs");
        // Go back to the text from before, as if the notice never came.
        view.update_in(cx, |view, window, cx| {
            view.editor
                .update(cx, |editor, cx| editor.set_value("", window, cx));
            view.saved = Some(Saved {
                text: String::new(),
                version: FileVersion::default(),
            });
            view.update_dirty(cx);
        });
        assert_eq!(text(&view, cx), "");
        let conn = view.read_with(cx, |view, _| view.connection.clone());
        conn.update(cx, |_, cx| cx.emit(ConnectionEvent::SessionsLoaded));
        wait_until(cx, |cx| view.read(cx).text(cx) == "theirs");
    }

    #[gpui::test]
    fn clearing_empties_the_thread_after_asking(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        write_elsewhere(&view, &other, "notes", cx);
        wait_until(cx, |cx| view.read(cx).text(cx) == "notes");

        view.update_in(cx, |view, window, cx| view.clear_clicked(window, cx));
        assert!(has_dialog(cx));
        assert_eq!(thread(&view, &other, cx).text, "notes");
        cx.update(|window, cx| window.close_dialog(cx));
        view.update_in(cx, |view, window, cx| view.clear(window, cx));
        wait_until(cx, |cx| !view.read(cx).saving);
        assert_eq!(thread(&view, &other, cx).text, "");
        assert_eq!(text(&view, cx), "");
        assert!(!view.read_with(cx, |view, _| view.is_dirty()));
    }

    #[gpui::test]
    fn a_thread_over_the_limit_is_not_sent(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, other, cx) = open(&agent, cx);
        let max = view.read_with(cx, |view, _| view.max_bytes) as usize;
        let set = |len: usize, cx: &mut VisualTestContext| {
            view.update_in(cx, |view, window, cx| {
                view.editor.update(cx, |editor, cx| {
                    editor.set_value("x".repeat(len), window, cx)
                });
                view.update_dirty(cx);
            });
        };
        set(max + 1, cx);
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        assert!(!save(&view, cx));
        assert_eq!(thread(&view, &other, cx).text, "");
        // Exactly at the limit is fine.
        set(max, cx);
        assert!(save(&view, cx));
        assert_eq!(thread(&view, &other, cx).text.len(), max);
    }
}
