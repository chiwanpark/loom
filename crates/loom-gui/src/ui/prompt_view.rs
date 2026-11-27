//! A read-only tab that shows what a session sends to the model besides the
//! conversation: the system prompt and the tools, as the session's next
//! model call would have them.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Editor, EditorState};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, h_flex, v_flex,
};
use loom_client::proto::{Notice, SessionId, SystemPrompt};

use crate::state::{AgentConnection, ConnectionEvent};
use crate::ui::code::{self, Highlighted};
use crate::ui::file_view::human_size;

pub(crate) const CONTEXT: &str = "SystemPromptView";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Prompt,
    Tools,
}

pub struct SystemPromptView {
    connection: Entity<AgentConnection>,
    session: SessionId,
    editor: Entity<EditorState>,
    /// `None` until the prompt is first read.
    prompt: Option<SystemPrompt>,
    /// Why the prompt could not be read the last time.
    error: Option<String>,
    loading: bool,
    section: Section,
    /// The tools whose description and parameters are shown.
    expanded: HashSet<String>,
    highlights: HashMap<String, Rc<Highlighted>>,
    focus: FocusHandle,
    _subscription: Subscription,
}

impl SystemPromptView {
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn new(
        connection: Entity<AgentConnection>,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("markdown")
                .line_number(false)
                .searchable(true)
                .soft_wrap(true)
        });
        // A new configuration can change the instructions and the tools, and
        // they may have changed while the link was down.
        let subscription =
            cx.subscribe_in(
                &connection,
                window,
                |this, _, event, window, cx| match event {
                    ConnectionEvent::Notice(Notice::ConfigReloaded { .. })
                    | ConnectionEvent::SessionsLoaded => this.reload(window, cx),
                    _ => {}
                },
            );
        let mut this = Self {
            connection,
            session,
            editor,
            prompt: None,
            error: None,
            loading: false,
            section: Section::Prompt,
            expanded: HashSet::new(),
            highlights: HashMap::new(),
            focus: cx.focus_handle(),
            _subscription: subscription,
        };
        this.reload(window, cx);
        this
    }

    /// Whether the prompt was read, and no read is under way.
    pub fn is_loaded(&self) -> bool {
        self.prompt.is_some() && !self.loading
    }

    pub fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    /// Reads the prompt again: the instructions, `AGENTS.md` files, and
    /// tools may have changed since.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        cx.notify();
        let task = self
            .connection
            .read(cx)
            .read_system_prompt(self.session.clone());
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Some(Ok(prompt)) => this.loaded(prompt, window, cx),
                    Some(Err(err)) => this.error = Some(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn loaded(&mut self, prompt: SystemPrompt, window: &mut Window, cx: &mut Context<Self>) {
        if *self.editor.read(cx).text() != prompt.text.as_str() {
            self.editor.update(cx, |editor, cx| {
                editor.set_value(prompt.text.clone(), window, cx)
            });
        }
        self.expanded
            .retain(|name| prompt.tools.iter().any(|tool| tool.name == *name));
        self.highlights.clear();
        self.error = None;
        self.prompt = Some(prompt);
    }

    fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    fn toggle_tool(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(name) {
            self.expanded.insert(name.to_owned());
        }
        cx.notify();
    }

    fn parameters(
        &mut self,
        name: &str,
        parameters: &str,
        theme: &gpui_component::Theme,
    ) -> Rc<Highlighted> {
        // The theme pointer changes when the theme does, which drops stale colors.
        let key = format!("{name}/{:p}", Arc::as_ptr(&theme.highlight_theme));
        self.highlights
            .entry(key)
            .or_insert_with(|| Rc::new(code::highlight(parameters, "json", &theme.highlight_theme)))
            .clone()
    }

    fn render_tools(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let tools = self
            .prompt
            .as_ref()
            .map(|prompt| prompt.tools.clone())
            .unwrap_or_default();
        let mut rows = Vec::new();
        for (ix, tool) in tools.iter().enumerate() {
            let expanded = self.expanded.contains(&tool.name);
            let summary = tool.description.lines().next().unwrap_or("").to_owned();
            let name = tool.name.clone();
            let header = h_flex()
                .id(("prompt-tool", ix))
                .debug_selector(move || format!("prompt-tool-{ix}"))
                .w_full()
                .gap_2()
                .px_3()
                .py_1()
                .cursor_pointer()
                .hover(|el| el.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_tool(&name, cx)))
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(theme.mono_font_family.clone())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tool.name.clone()),
                )
                .when(!expanded, |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(theme.muted_foreground)
                            .child(summary),
                    )
                });
            let mut row = v_flex().w_full().child(header);
            if expanded {
                let parameters = self.parameters(&tool.name, &tool.parameters, &theme);
                row = row.child(
                    v_flex()
                        .id(("prompt-tool-details", ix))
                        .debug_selector(move || format!("prompt-tool-details-{ix}"))
                        .w_full()
                        .gap_2()
                        .pl_8()
                        .pr_3()
                        .pb_2()
                        .child(
                            div()
                                .w_full()
                                .whitespace_normal()
                                .child(tool.description.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child("Parameters"),
                        )
                        .child(code::code_view(&parameters, None, &theme)),
                );
            }
            rows.push(row);
        }
        v_flex()
            .id("prompt-tools")
            .flex_1()
            .min_h_0()
            .py_1()
            .text_sm()
            .overflow_y_scroll()
            .when(tools.is_empty(), |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_color(theme.muted_foreground)
                        .child("The model has no tools."),
                )
            })
            .children(rows)
    }
}

impl Focusable for SystemPromptView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SystemPromptView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let detail = match (&self.prompt, self.loading) {
            (_, true) => "loading…".to_owned(),
            (Some(prompt), false) => human_size(prompt.text.len() as u64),
            (None, false) => "error".to_owned(),
        };
        let tools = self.prompt.as_ref().map_or(0, |prompt| prompt.tools.len());
        let warnings = self
            .prompt
            .as_ref()
            .map(|prompt| prompt.warnings.clone())
            .unwrap_or_default();
        let section = self.section;
        let body = match (&self.prompt, &self.error) {
            (None, Some(error)) => div()
                .p_4()
                .text_color(theme.muted_foreground)
                .child(format!("Could not read the system prompt: {error}"))
                .into_any_element(),
            (None, None) => div()
                .p_4()
                .text_color(theme.muted_foreground)
                .child("Reading the system prompt…")
                .into_any_element(),
            (Some(_), _) if section == Section::Tools => self.render_tools(cx).into_any_element(),
            (Some(_), _) => div()
                .flex_1()
                .min_h_0()
                .child(
                    Editor::new(&self.editor)
                        .readonly(true)
                        .bordered(false)
                        .appearance(false)
                        .h_full()
                        .small(),
                )
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .key_context(CONTEXT)
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
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                Button::new("prompt-section-prompt")
                                    .ghost()
                                    .xsmall()
                                    .label("Prompt")
                                    .selected(section == Section::Prompt)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.show(Section::Prompt, cx)
                                        }),
                                    ),
                            )
                            .child(
                                Button::new("prompt-section-tools")
                                    .ghost()
                                    .xsmall()
                                    .label(SharedString::from(format!("Tools ({tools})")))
                                    .selected(section == Section::Tools)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.show(Section::Tools, cx)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                    .child(
                        Button::new("reload-prompt")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::RefreshCw))
                            .disabled(self.loading)
                            .tooltip("Read the system prompt again")
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    ),
            )
            .when(!warnings.is_empty(), |el| {
                el.child(
                    v_flex()
                        .id("prompt-warnings")
                        .debug_selector(|| "prompt-warnings".into())
                        .w_full()
                        .px_3()
                        .py_1()
                        .gap_1()
                        .text_xs()
                        .text_color(theme.warning)
                        .border_b_1()
                        .border_color(theme.border)
                        .children(warnings),
                )
            })
            .when_some(
                self.error.clone().filter(|_| self.prompt.is_some()),
                |el, error| {
                    el.child(
                        div()
                            .w_full()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(format!("Could not read the system prompt again: {error}")),
                    )
                },
            )
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{TestAppContext, VisualTestContext};
    use gpui_component::Root;

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    fn open<'a>(
        agent: &TestAgent,
        cx: &'a mut TestAppContext,
    ) -> (Entity<SystemPromptView>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let conn = agent.connect(cx);
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
                let view = cx.new(|cx| SystemPromptView::new(conn, session, window, cx));
                *slot.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            }
        });
        let view = slot.borrow_mut().take().unwrap();
        wait_until(cx, |cx| view.read(cx).prompt.is_some());
        (view, cx)
    }

    fn text(view: &Entity<SystemPromptView>, cx: &mut VisualTestContext) -> String {
        view.read_with(cx, |view, cx| view.editor.read(cx).value().to_string())
    }

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no {selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn shows_the_prompt_and_reads_it_again(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("AGENTS.md", "Use tabs.");
        let (view, cx) = open(&agent, cx);
        let shown = text(&view, cx);
        assert!(shown.starts_with("You are Loom"), "{shown}");
        assert!(shown.contains("Use tabs."), "{shown}");
        assert!(cx.debug_bounds("prompt-warnings").is_none());

        agent.write("AGENTS.md", "Use spaces.");
        view.update_in(cx, |view, window, cx| view.reload(window, cx));
        wait_until(cx, |cx| {
            !view.read(cx).loading
                && view
                    .read(cx)
                    .editor
                    .read(cx)
                    .text()
                    .to_string()
                    .contains("Use spaces.")
        });
    }

    #[gpui::test]
    fn tools_open_to_show_their_parameters(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (view, cx) = open(&agent, cx);
        let names: Vec<String> = view.read_with(cx, |view, _| {
            view.prompt
                .as_ref()
                .unwrap()
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect()
        });
        assert!(names.iter().any(|name| name == "read"), "{names:?}");

        view.update(cx, |view, cx| view.show(Section::Tools, cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("prompt-tool-0").is_some());
        assert!(cx.debug_bounds("prompt-tool-details-0").is_none());
        click("prompt-tool-0", cx);
        assert!(cx.debug_bounds("prompt-tool-details-0").is_some());
        assert!(view.read_with(cx, |view, _| view.expanded.contains(&names[0])));
        click("prompt-tool-0", cx);
        assert!(cx.debug_bounds("prompt-tool-details-0").is_none());
    }
}
