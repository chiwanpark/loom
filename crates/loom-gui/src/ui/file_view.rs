use std::path::PathBuf;

use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Subscription, Task, Window,
    div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Editor, EditorState, InputEvent};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, h_flex, v_flex,
};
use loom_client::proto::{ErrorCode, FileContent, FileVersion};

use crate::state::AgentConnection;
use crate::ui::workspace::{Save, ToggleLineWrap};
use crate::ui::{dialogs, download, notify};

pub(crate) const CONTEXT: &str = "FileView";

enum Status {
    Loading,
    Loaded {
        size: u64,
        truncated: bool,
        /// Why the file cannot be edited, if it cannot.
        readonly: Option<&'static str>,
    },
    Binary {
        size: u64,
    },
    Failed(String),
}

/// What the file holds on the agent, as far as this view knows: the text
/// it last read or saved, and the version that text has.
struct Saved {
    text: String,
    version: FileVersion,
}

pub struct FileView {
    connection: Entity<AgentConnection>,
    relative: String,
    path: PathBuf,
    editor: Entity<EditorState>,
    status: Status,
    /// Set only while the file can be edited.
    saved: Option<Saved>,
    /// Whether the editor's text differs from `saved`.
    dirty: bool,
    loading: bool,
    saving: bool,
    wrap: bool,
    focus: FocusHandle,
    _subscription: Subscription,
}

/// Why a file that was read cannot be edited and saved back, if it cannot.
fn readonly_reason(content: &FileContent) -> Option<&'static str> {
    if content.truncated {
        Some("read-only, too large to edit")
    } else if !content.valid_utf8 {
        Some("read-only, not UTF-8")
    } else {
        None
    }
}

pub(crate) fn language(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_lowercase();
    match lower.as_str() {
        "makefile" | "gnumakefile" => return "make".into(),
        "dockerfile" => return "dockerfile".into(),
        "cargo.lock" => return "toml".into(),
        _ => {}
    }
    let ext = match lower.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => return "text".into(),
    };
    match ext {
        "h" => "c",
        "hpp" | "hh" | "cc" | "cxx" => "cpp",
        "jsx" | "mjs" | "cjs" => "javascript",
        "mts" | "cts" => "typescript",
        "zsh" | "bash" => "bash",
        "htm" => "html",
        "txt" | "log" => "text",
        other => other,
    }
    .to_string()
}

pub(crate) fn human_size(size: u64) -> String {
    match size {
        0..1024 => format!("{size} B"),
        1024..1_048_576 => format!("{:.1} KB", size as f64 / 1024.),
        1_048_576..1_073_741_824 => format!("{:.1} MB", size as f64 / 1_048_576.),
        _ => format!("{:.1} GB", size as f64 / 1_073_741_824.),
    }
}

/// The header button that turns line wrapping on and off, shared with the
/// diff view.
pub(crate) fn wrap_button(id: &'static str, wrap: bool) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::new(gpui_kit_assets::IconName::TextWrap))
        .selected(wrap)
        .toggled(wrap)
        .tooltip_with_action(
            if wrap {
                "Stop wrapping lines"
            } else {
                "Wrap lines"
            },
            &ToggleLineWrap,
            None,
        )
}

impl FileView {
    pub fn new(
        connection: Entity<AgentConnection>,
        relative: String,
        wrap: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path = connection.read(cx).info.workspace.join(&relative);
        let language = language(&relative);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(language)
                .line_number(true)
                .searchable(true)
                .soft_wrap(wrap)
        });
        let subscription = cx.subscribe(&editor, |this: &mut Self, _, event, cx| {
            if let InputEvent::Change = event {
                this.update_dirty(cx);
            }
        });
        let mut this = Self {
            connection,
            relative,
            path,
            editor,
            status: Status::Loading,
            saved: None,
            dirty: false,
            loading: false,
            saving: false,
            wrap,
            focus: cx.focus_handle(),
            _subscription: subscription,
        };
        this.load(false, window, cx);
        this
    }

    /// Whether the editor has changes that are not saved.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the file was read whole, as text that can be saved back.
    pub fn is_editable(&self) -> bool {
        self.saved.is_some()
    }

    pub fn is_saving(&self) -> bool {
        self.saving
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

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn title(&self) -> String {
        self.relative
            .rsplit('/')
            .next()
            .unwrap_or(&self.relative)
            .to_string()
    }

    pub fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        let wrap = self.wrap;
        self.editor
            .update(cx, |editor, cx| editor.set_soft_wrap(wrap, window, cx));
        cx.notify();
    }

    /// Reads the file again. Unsaved changes in the editor stay.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load(false, window, cx);
    }

    /// The reload button: asks before throwing away unsaved changes.
    fn reload_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.dirty {
            self.load(false, window, cx);
            return;
        }
        let this = cx.entity().downgrade();
        dialogs::confirm(
            "Discard your changes?",
            format!(
                "Reloading {} from disk throws away the changes you have not saved.",
                self.relative
            ),
            "Discard and reload",
            move |window, cx| {
                let _ = this.update(cx, |this, cx| this.load(true, window, cx));
            },
            window,
            cx,
        );
    }

    /// Reads the file. The text replaces the editor's only when the editor
    /// has no unsaved changes by the time it arrives, or when `discard` says
    /// to throw them away.
    fn load(&mut self, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let task = self.connection.read(cx).read_file(self.relative.clone());
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Some(Ok(_)) if this.dirty && !discard => {}
                    Some(Ok(content)) => this.loaded(content, window, cx),
                    Some(Err(err)) => {
                        this.status = Status::Failed(err.to_string());
                        this.saved = None;
                        this.dirty = false;
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn loaded(&mut self, content: FileContent, window: &mut Window, cx: &mut Context<Self>) {
        if content.binary {
            self.status = Status::Binary { size: content.size };
            self.saved = None;
            self.dirty = false;
            return;
        }
        let readonly = readonly_reason(&content);
        if *self.editor.read(cx).text() != content.text.as_str() {
            // Keeps the undo history when the file did not change.
            self.editor.update(cx, |editor, cx| {
                editor.set_value(content.text.clone(), window, cx)
            });
        }
        self.status = Status::Loaded {
            size: content.size,
            truncated: content.truncated,
            readonly,
        };
        self.saved = readonly.is_none().then_some(Saved {
            text: content.text,
            version: content.version,
        });
        self.dirty = false;
    }

    /// Saves the editor's text to the file. The task says whether the file
    /// holds the text now. A file that changed on disk since it was read is
    /// not overwritten; the edits stay in the editor.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        let Some(saved) = &self.saved else {
            return Task::ready(false);
        };
        if !self.dirty {
            return Task::ready(true);
        }
        if self.saving {
            return Task::ready(false);
        }
        self.saving = true;
        cx.notify();
        let text = self.editor.read(cx).value().to_string();
        let task =
            self.connection
                .read(cx)
                .write_file(self.relative.clone(), text.clone(), saved.version);
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                cx.notify();
                match result {
                    Some(Ok(version)) => {
                        if let Status::Loaded { size, .. } = &mut this.status {
                            *size = text.len() as u64;
                        }
                        this.saved = Some(Saved { text, version });
                        this.update_dirty(cx);
                        this.connection
                            .update(cx, |conn, cx| conn.refresh_changes(cx));
                        true
                    }
                    Some(Err(err)) => {
                        let message = match err.api() {
                            Some(api) if api.code == ErrorCode::Conflict => format!(
                                "{} was not saved: it changed on disk after it was opened. \
                                 Copy your changes somewhere, then reload the file.",
                                this.relative
                            ),
                            _ => format!("Could not save {}: {err}", this.relative),
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
}

impl Focusable for FileView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FileView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let detail = match &self.status {
            Status::Loading => "loading…".to_string(),
            Status::Loaded {
                size,
                truncated,
                readonly,
            } => {
                let mut detail = human_size(*size);
                if *truncated {
                    detail.push_str(" · showing the first 2 MB");
                }
                if let Some(reason) = readonly {
                    detail.push_str(" · ");
                    detail.push_str(reason);
                }
                if self.saving {
                    detail.push_str(" · saving…");
                }
                detail
            }
            Status::Binary { size } => format!("{} · binary", human_size(*size)),
            Status::Failed(_) => "error".to_string(),
        };
        let message = match &self.status {
            Status::Binary { .. } => Some("This is a binary file and cannot be shown.".to_string()),
            Status::Failed(err) => Some(format!("Could not read the file: {err}")),
            _ => None,
        };
        let editable = self.saved.is_some();
        v_flex()
            .size_full()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &ToggleLineWrap, window, cx| this.toggle_wrap(window, cx)),
            )
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save_clicked(window, cx)))
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .id("file-title")
                                    .debug_selector(|| "file-title".into())
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.relative.clone()),
                            )
                            .when(self.dirty, |el| {
                                el.child(
                                    div()
                                        .id("file-unsaved")
                                        .debug_selector(|| "file-unsaved".into())
                                        .flex_none()
                                        .text_color(theme.muted_foreground)
                                        .child("●"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                    .child(
                        wrap_button("file-wrap", self.wrap).on_click(
                            cx.listener(|this, _, window, cx| this.toggle_wrap(window, cx)),
                        ),
                    )
                    .when(editable, |el| {
                        el.child(
                            Button::new("save-file")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(gpui_kit_assets::IconName::Save))
                                .disabled(!self.dirty || self.saving)
                                .tooltip_with_action("Save", &Save, Some(CONTEXT))
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.save_clicked(window, cx)
                                    }),
                                ),
                        )
                    })
                    .when(self.connection.read(cx).downloads_supported(), |el| {
                        el.child(
                            Button::new("download-file")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(gpui_kit_assets::IconName::Download))
                                .tooltip("Download…")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    download::prompt(
                                        this.connection.clone(),
                                        this.relative.clone(),
                                        window,
                                        cx,
                                    )
                                })),
                        )
                    })
                    .child(
                        Button::new("reload-file")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::RefreshCw))
                            .tooltip("Reload from disk")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.reload_clicked(window, cx)),
                            ),
                    ),
            )
            .when_some(message, |el, message| {
                el.child(
                    div()
                        .p_4()
                        .text_color(theme.muted_foreground)
                        .child(message),
                )
            })
            .when(matches!(self.status, Status::Loaded { .. }), |el| {
                el.child(
                    div().flex_1().min_h_0().child(
                        Editor::new(&self.editor)
                            .readonly(!editable)
                            .bordered(false)
                            .appearance(false)
                            .h_full()
                            .small(),
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;
    use std::rc::Rc;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::{Root, WindowExt};

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    fn open<'a>(
        agent: &TestAgent,
        path: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<FileView>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let conn = agent.connect(cx);
        let slot = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let (slot, path) = (slot.clone(), path.to_string());
            move |window, cx| {
                let view = cx.new(|cx| FileView::new(conn, path, false, window, cx));
                *slot.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let view = slot.borrow_mut().take().unwrap();
        wait_until(cx, |cx| !matches!(view.read(cx).status, Status::Loading));
        view.update_in(cx, |view, window, cx| {
            view.editor_focus(cx).focus(window, cx)
        });
        cx.run_until_parked();
        (view, cx)
    }

    fn text(view: &Entity<FileView>, cx: &mut VisualTestContext) -> String {
        view.read_with(cx, |view, cx| view.editor.read(cx).value().to_string())
    }

    fn dirty(view: &Entity<FileView>, cx: &mut VisualTestContext) -> bool {
        view.read_with(cx, |view, _| view.is_dirty())
    }

    #[gpui::test]
    fn edits_are_saved_with_ctrl_s_and_undo_tracks_the_saved_text(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("run.sh", "one\r\ntwo\r\n");
        let file = agent.workspace.join("run.sh");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        let (view, cx) = open(&agent, "run.sh", cx);
        assert_eq!(text(&view, cx), "one\r\ntwo\r\n");
        assert!(!dirty(&view, cx));
        assert!(cx.debug_bounds("file-unsaved").is_none());

        cx.simulate_input("x");
        assert!(dirty(&view, cx));
        assert!(cx.debug_bounds("file-unsaved").is_some());
        // Undoing back to the saved text is not a change.
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(text(&view, cx), "one\r\ntwo\r\n");
        assert!(!dirty(&view, cx));
        assert!(cx.debug_bounds("file-unsaved").is_none());
        cx.simulate_keystrokes("ctrl-y");
        assert!(dirty(&view, cx));

        cx.simulate_keystrokes("ctrl-s");
        wait_until(cx, |cx| !view.read(cx).is_dirty());
        assert_eq!(agent.read("run.sh"), b"xone\r\ntwo\r\n");
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
        assert!(cx.debug_bounds("file-unsaved").is_none());

        // Saving keeps the undo history, and so does reloading a file that
        // did not change. The next save builds on this one.
        view.update_in(cx, |view, window, cx| view.reload(window, cx));
        wait_until(cx, |cx| !view.read(cx).loading);
        cx.simulate_keystrokes("ctrl-z");
        assert_eq!(text(&view, cx), "one\r\ntwo\r\n");
        assert!(dirty(&view, cx));
        cx.simulate_keystrokes("ctrl-s");
        wait_until(cx, |cx| !view.read(cx).is_dirty());
        assert_eq!(agent.read("run.sh"), b"one\r\ntwo\r\n");
    }

    #[gpui::test]
    fn the_unsaved_marker_sits_after_the_name_without_growing_the_header(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (_view, cx) = open(&agent, "a.txt", cx);
        let before = cx.debug_bounds("file-title").unwrap();
        cx.simulate_input("x");
        let title = cx.debug_bounds("file-title").unwrap();
        let dot = cx.debug_bounds("file-unsaved").unwrap();
        assert_eq!(title, before, "the name stays where it was");
        assert!(dot.left() >= title.right(), "{dot:?} after {title:?}");
        assert!(
            dot.top() < title.bottom() && dot.bottom() > title.top(),
            "{dot:?} on the same line as {title:?}"
        );
    }

    #[gpui::test]
    fn a_file_changed_on_disk_is_not_overwritten(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "mine\n");
        let (view, cx) = open(&agent, "a.txt", cx);
        cx.simulate_input("edit ");
        agent.write("a.txt", "theirs\n");

        let saved = view.update_in(cx, |view, window, cx| view.save(window, cx));
        wait_until(cx, |cx| !view.read(cx).saving);
        assert!(!futures::executor::block_on(saved));
        assert_eq!(agent.read("a.txt"), b"theirs\n");
        assert_eq!(text(&view, cx), "edit mine\n");
        assert!(dirty(&view, cx));
    }

    #[gpui::test]
    fn reloading_keeps_unsaved_changes_unless_told_to_discard_them(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (view, cx) = open(&agent, "a.txt", cx);
        cx.simulate_input("x");
        agent.write("a.txt", "two\n");

        // Opening the file again reloads it, which must not wipe the edits.
        view.update_in(cx, |view, window, cx| view.reload(window, cx));
        wait_until(cx, |cx| !view.read(cx).loading);
        assert_eq!(text(&view, cx), "xone\n");
        assert!(dirty(&view, cx));
        // Nor does a read that was under way when the editing started.
        view.update_in(cx, |view, window, cx| view.load(false, window, cx));
        wait_until(cx, |cx| !view.read(cx).loading);
        assert_eq!(text(&view, cx), "xone\n");

        // The reload button asks first.
        view.update_in(cx, |view, window, cx| view.reload_clicked(window, cx));
        assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
        assert_eq!(text(&view, cx), "xone\n");
        view.update_in(cx, |view, window, cx| view.load(true, window, cx));
        wait_until(cx, |cx| !view.read(cx).loading);
        assert_eq!(text(&view, cx), "two\n");
        assert!(!dirty(&view, cx));
    }

    #[gpui::test]
    fn text_that_cannot_be_saved_back_is_read_only(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("latin1.txt", b"caf\xe9\n");
        let (view, cx) = open(&agent, "latin1.txt", cx);
        let before = text(&view, cx);
        cx.simulate_input("x");
        cx.simulate_keystrokes("ctrl-s");
        cx.run_until_parked();
        assert_eq!(text(&view, cx), before);
        assert!(!dirty(&view, cx));
        assert_eq!(agent.read("latin1.txt"), b"caf\xe9\n");
        view.read_with(cx, |view, _| {
            assert!(matches!(
                view.status,
                Status::Loaded {
                    readonly: Some("read-only, not UTF-8"),
                    ..
                }
            ));
        });
    }

    #[test]
    fn picks_languages_from_names_and_extensions() {
        assert_eq!(language("src/main.rs"), "rs");
        assert_eq!(language("include/a.h"), "c");
        assert_eq!(language("Makefile"), "make");
        assert_eq!(language("README"), "text");
        assert_eq!(language("app/view.JSX"), "javascript");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 << 20), "3.0 MB");
        assert_eq!(human_size(5 << 30), "5.0 GB");
    }

    #[test]
    fn only_whole_utf8_text_can_be_edited() {
        let text = FileContent {
            valid_utf8: true,
            ..FileContent::default()
        };
        assert_eq!(readonly_reason(&text), None);
        let truncated = FileContent {
            truncated: true,
            ..text.clone()
        };
        assert_eq!(
            readonly_reason(&truncated),
            Some("read-only, too large to edit")
        );
        let latin1 = FileContent {
            valid_utf8: false,
            ..text
        };
        assert_eq!(readonly_reason(&latin1), Some("read-only, not UTF-8"));
    }
}
