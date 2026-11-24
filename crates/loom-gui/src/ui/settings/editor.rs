use gpui::ParentElement;
use gpui::{App, AppContext, Context, Entity, IntoElement, Styled, Window, div, px};
use gpui_component::Sizable;
use gpui_component::input::{Editor, EditorState};
use loom_view_model::tool_details::language;

pub struct TextFile {
    pub path: String,
    pub editor: Entity<EditorState>,
    pub saved: String,
}

impl TextFile {
    pub fn new<T: 'static>(
        path: impl Into<String>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let path = path.into();
        let language = language(&path);
        let value = text.to_owned();
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language(language)
                .line_number(true)
                .searchable(true)
                .soft_wrap(false);
            state.set_value(value, window, cx);
            state
        });
        Self {
            path,
            editor,
            saved: text.to_owned(),
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.text(cx) != self.saved
    }

    pub fn mark_saved(&mut self, cx: &App) {
        self.saved = self.text(cx);
    }

    pub fn render(&self, height: f32) -> impl IntoElement {
        div()
            .w_full()
            .h(px(height))
            .child(Editor::new(&self.editor).h_full().small())
    }
}
