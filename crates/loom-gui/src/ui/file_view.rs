use std::path::PathBuf;

use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Editor, EditorState};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};

use crate::state::AgentConnection;

enum Status {
    Loading,
    Loaded { size: u64, truncated: bool },
    Binary { size: u64 },
    Failed(String),
}

pub struct FileView {
    connection: Entity<AgentConnection>,
    relative: String,
    path: PathBuf,
    editor: Entity<EditorState>,
    status: Status,
    loading: bool,
    focus: FocusHandle,
}

fn language(path: &str) -> String {
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

fn human_size(size: u64) -> String {
    match size {
        0..1024 => format!("{size} B"),
        1024..1_048_576 => format!("{:.1} KB", size as f64 / 1024.),
        _ => format!("{:.1} MB", size as f64 / 1_048_576.),
    }
}

impl FileView {
    pub fn new(
        connection: Entity<AgentConnection>,
        relative: String,
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
                .soft_wrap(false)
        });
        let mut this = Self {
            connection,
            relative,
            path,
            editor,
            status: Status::Loading,
            loading: false,
            focus: cx.focus_handle(),
        };
        this.reload(window, cx);
        this
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

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    Some(Ok(content)) if content.binary => {
                        this.status = Status::Binary { size: content.size };
                    }
                    Some(Ok(content)) => {
                        let current = this.editor.read(cx).value();
                        if current.as_ref() != content.text.as_str() {
                            this.editor.update(cx, |editor, cx| {
                                editor.set_value(content.text, window, cx)
                            });
                        }
                        this.status = Status::Loaded {
                            size: content.size,
                            truncated: content.truncated,
                        };
                    }
                    Some(Err(err)) => this.status = Status::Failed(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
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
            Status::Loaded { size, truncated } => {
                if *truncated {
                    format!("{} · showing the first 2 MB", human_size(*size))
                } else {
                    human_size(*size)
                }
            }
            Status::Binary { size } => format!("{} · binary", human_size(*size)),
            Status::Failed(_) => "error".to_string(),
        };
        let message = match &self.status {
            Status::Binary { .. } => Some("This is a binary file and cannot be shown.".to_string()),
            Status::Failed(err) => Some(format!("Could not read the file: {err}")),
            _ => None,
        };
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.relative.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                    .child(
                        Button::new("reload-file")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::RefreshCw))
                            .tooltip("Reload from disk")
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
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
                            .readonly(true)
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
    use super::*;

    #[test]
    fn picks_languages_from_names_and_extensions() {
        assert_eq!(language("src/main.rs"), "rs");
        assert_eq!(language("include/a.h"), "c");
        assert_eq!(language("Makefile"), "make");
        assert_eq!(language("README"), "text");
        assert_eq!(language("app/view.JSX"), "javascript");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
    }
}
