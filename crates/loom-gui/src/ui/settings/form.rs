use std::future::Future;
use std::rc::Rc;

use gpui::{
    App, AppContext, AsyncWindowContext, ClickEvent, Context, Entity, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Styled, Subscription, WeakEntity, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};

pub enum Kind {
    Text,
    Secret,
    Lines(usize),
    Choice(Vec<String>),
    Check,
}

pub struct Field {
    label: SharedString,
    placeholder: SharedString,
    help: Option<SharedString>,
    value: String,
    kind: Kind,
}

impl Field {
    fn new(label: impl Into<SharedString>, kind: Kind) -> Self {
        Self {
            label: label.into(),
            placeholder: SharedString::default(),
            help: None,
            value: String::new(),
            kind,
        }
    }

    pub fn text(label: impl Into<SharedString>) -> Self {
        Self::new(label, Kind::Text)
    }

    pub fn secret(label: impl Into<SharedString>) -> Self {
        Self::new(label, Kind::Secret)
    }

    pub fn lines(label: impl Into<SharedString>, rows: usize) -> Self {
        Self::new(label, Kind::Lines(rows))
    }

    pub fn choice(label: impl Into<SharedString>, options: Vec<String>) -> Self {
        let value = options.first().cloned().unwrap_or_default();
        Self::new(label, Kind::Choice(options)).value(value)
    }

    pub fn check(label: impl Into<SharedString>, checked: bool) -> Self {
        Self::new(label, Kind::Check).value(if checked { "true" } else { "" })
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn help(mut self, help: impl Into<SharedString>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }
}

pub struct Values(Vec<String>);

impl Values {
    pub fn text(&self, ix: usize) -> String {
        self.0
            .get(ix)
            .map(|v| v.trim().to_owned())
            .unwrap_or_default()
    }

    pub fn optional(&self, ix: usize) -> Option<String> {
        Some(self.text(ix)).filter(|v| !v.is_empty())
    }

    pub fn raw(&self, ix: usize) -> String {
        self.0.get(ix).cloned().unwrap_or_default()
    }

    pub fn checked(&self, ix: usize) -> bool {
        self.0.get(ix).is_some_and(|v| v == "true")
    }

    pub fn lines(&self, ix: usize) -> Vec<String> {
        self.raw(ix)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

enum Control {
    Input(Entity<InputState>),
    Lines(Entity<TextareaState>),
    Choice(Vec<String>, String),
    Check(bool),
}

pub type Submit = Rc<dyn Fn(Values, Done, &mut Window, &mut App)>;

pub struct FormView {
    description: Option<SharedString>,
    fields: Vec<(Field, Control)>,
    submit_label: SharedString,
    submit: Submit,
    busy: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
pub struct Done(WeakEntity<FormView>);

impl Done {
    pub fn finish(self, result: Result<(), String>, window: &mut Window, cx: &mut App) {
        let _ = self.0.update(cx, |form, cx| {
            form.busy = false;
            match result {
                Ok(()) => window.close_dialog(cx),
                Err(error) => form.error = Some(error),
            }
            cx.notify();
        });
    }

    pub fn fail(self, error: impl Into<String>, window: &mut Window, cx: &mut App) {
        self.finish(Err(error.into()), window, cx);
    }

    pub fn wait<T: 'static>(
        self,
        task: impl Future<Output = Result<T, String>> + 'static,
        then: impl FnOnce(T, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) {
        window
            .spawn(cx, async move |cx: &mut AsyncWindowContext| {
                let result = task.await;
                let _ = cx.update(|window, cx| match result {
                    Ok(value) => {
                        self.finish(Ok(()), window, cx);
                        then(value, window, cx);
                    }
                    Err(error) => self.finish(Err(error), window, cx),
                });
            })
            .detach();
    }
}

impl FormView {
    fn values(&self, cx: &App) -> Values {
        Values(
            self.fields
                .iter()
                .map(|(_, control)| match control {
                    Control::Input(input) => input.read(cx).value().to_string(),
                    Control::Lines(input) => input.read(cx).value().to_string(),
                    Control::Choice(_, value) => value.clone(),
                    Control::Check(checked) => if *checked { "true" } else { "" }.to_owned(),
                })
                .collect(),
        )
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let values = self.values(cx);
        let done = Done(cx.entity().downgrade());
        let submit = self.submit.clone();
        window.defer(cx, move |window, cx| submit(values, done, window, cx));
    }

    fn render_control(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (field, control) = &self.fields[ix];
        match control {
            Control::Input(input) => Input::new(input).into_any_element(),
            Control::Lines(input) => Textarea::new(input).into_any_element(),
            Control::Choice(options, value) => {
                let options = options.clone();
                let current = value.clone();
                let this = cx.entity().downgrade();
                Button::new(SharedString::from(format!("form-choice-{ix}")))
                    .outline()
                    .small()
                    .label(if current.is_empty() {
                        "None".to_owned()
                    } else {
                        current.clone()
                    })
                    .dropdown_menu(move |mut menu, _, _| {
                        for option in &options {
                            let this = this.clone();
                            let picked = option.clone();
                            menu = menu.item(
                                PopupMenuItem::new(if option.is_empty() {
                                    "None".to_owned()
                                } else {
                                    option.clone()
                                })
                                .checked(*option == current)
                                .on_click(
                                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                                        let _ = this.update(cx, |form, cx| {
                                            if let Some((_, Control::Choice(_, value))) =
                                                form.fields.get_mut(ix)
                                            {
                                                *value = picked.clone();
                                            }
                                            cx.notify();
                                        });
                                    },
                                ),
                            );
                        }
                        menu
                    })
                    .into_any_element()
            }
            Control::Check(checked) => {
                Checkbox::new(SharedString::from(format!("form-check-{ix}")))
                    .label(field.label.clone())
                    .checked(*checked)
                    .on_click(cx.listener(move |form, checked: &bool, _, cx| {
                        if let Some((_, Control::Check(value))) = form.fields.get_mut(ix) {
                            *value = *checked;
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
        }
    }
}

impl Render for FormView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut body = v_flex()
            .gap_3()
            .when_some(self.description.clone(), |el, text| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(text),
                )
            });
        for ix in 0..self.fields.len() {
            let control = self.render_control(ix, cx);
            let (field, kind) = &self.fields[ix];
            let check = matches!(kind, Control::Check(_));
            body = body.child(
                v_flex()
                    .gap_1()
                    .when(!check, |el| {
                        el.child(div().text_sm().child(field.label.clone()))
                    })
                    .child(control)
                    .when_some(field.help.clone(), |el, help| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(help),
                        )
                    }),
            );
        }
        body.when_some(self.error.clone(), |el, error| {
            el.child(
                div()
                    .debug_selector(|| "form-error".into())
                    .text_sm()
                    .text_color(theme.danger)
                    .whitespace_normal()
                    .child(error),
            )
        })
        .child(
            h_flex().justify_end().gap_2().child(
                div().debug_selector(|| "form-submit".into()).child(
                    Button::new("form-submit")
                        .primary()
                        .label(self.submit_label.clone())
                        .loading(self.busy)
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
            ),
        )
    }
}

pub fn open(
    title: impl Into<SharedString>,
    description: Option<String>,
    fields: Vec<Field>,
    submit_label: impl Into<SharedString>,
    submit: impl Fn(Values, Done, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| {
        let mut subscriptions = Vec::new();
        let fields = fields
            .into_iter()
            .map(|field| {
                let control = match &field.kind {
                    Kind::Text | Kind::Secret => {
                        let secret = matches!(field.kind, Kind::Secret);
                        let input = cx.new(|cx| {
                            let mut state = InputState::new(window, cx)
                                .placeholder(field.placeholder.clone())
                                .masked(secret);
                            state.set_value(field.value.clone(), window, cx);
                            state
                        });
                        subscriptions.push(cx.subscribe_in(
                            &input,
                            window,
                            |this: &mut FormView, _, event: &InputEvent, window, cx| {
                                if let InputEvent::PressEnter { .. } = event {
                                    this.submit(window, cx);
                                }
                            },
                        ));
                        Control::Input(input)
                    }
                    Kind::Lines(rows) => {
                        let rows = *rows;
                        Control::Lines(cx.new(|cx| {
                            let mut state = TextareaState::new(window, cx)
                                .auto_grow(rows, rows.max(12))
                                .placeholder(field.placeholder.clone());
                            state.set_value(field.value.clone(), window, cx);
                            state
                        }))
                    }
                    Kind::Choice(options) => Control::Choice(options.clone(), field.value.clone()),
                    Kind::Check => Control::Check(field.value == "true"),
                };
                (field, control)
            })
            .collect();
        FormView {
            description: description.map(SharedString::from),
            fields,
            submit_label: submit_label.into(),
            submit: Rc::new(submit),
            busy: false,
            error: None,
            _subscriptions: subscriptions,
        }
    });
    let first = view.read(cx).fields.iter().find_map(|(_, c)| match c {
        Control::Input(input) => Some(input.clone()),
        _ => None,
    });
    let title = title.into();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(px(560.)).child(view.clone())
    });
    if let Some(input) = first {
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
    }
}
