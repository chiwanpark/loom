use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{EditorState, Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::{ActiveTheme, Disableable, Selectable, Sizable, h_flex, v_flex};
use loom_config::{ConfigDocument, PolicyRule, valid_variable_name};
use loom_view_model::agent_settings::{self as settings, Field, Kind, POLICY_ACTIONS, SECTIONS};
use serde_json::Value;

use super::form::{self, Field as FormField};
use super::{danger, detail, small_button, warning};
use crate::oracle::OracleLink;

enum TextControl {
    Line(Entity<InputState>),
    Lines(Entity<TextareaState>),
}

pub struct AgentForm {
    link: Entity<OracleLink>,
    yaml: Entity<EditorState>,
    defaults: ConfigDocument,
    doc: Result<ConfigDocument, String>,
    comments: bool,
    section: usize,
    texts: HashMap<&'static str, TextControl>,
    errors: HashMap<&'static str, String>,
    _subscriptions: Vec<Subscription>,
}

fn find_field(path: &str) -> Option<&'static Field> {
    SECTIONS
        .iter()
        .flat_map(|section| section.fields)
        .find(|field| field.path == path)
}

fn option_label(option: &str, default: Option<&str>) -> String {
    if default == Some(option) {
        format!("{option} (default)")
    } else {
        option.to_owned()
    }
}

fn id(prefix: &str, key: impl std::fmt::Display) -> SharedString {
    SharedString::from(format!("agent-{prefix}-{key}"))
}

impl AgentForm {
    pub fn new(
        link: Entity<OracleLink>,
        yaml: Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut form = Self {
            link,
            yaml,
            defaults: settings::preset_defaults(),
            doc: Ok(ConfigDocument::default()),
            comments: false,
            section: 0,
            texts: HashMap::new(),
            errors: HashMap::new(),
            _subscriptions: Vec::new(),
        };
        form.reload(window, cx);
        form
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.yaml.read(cx).value().to_string();
        self.comments = settings::has_comments(&text);
        self.doc = ConfigDocument::parse(&text, "agent.yaml").map_err(|err| err.to_string());
        self.errors.clear();
        self.texts.clear();
        self._subscriptions.clear();
        if let Ok(doc) = &self.doc {
            for field in SECTIONS.iter().flat_map(|section| section.fields) {
                if !field.kind.is_text() {
                    continue;
                }
                let path = field.path;
                let value = settings::text_of(field.kind, doc.get(path));
                let placeholder = settings::placeholder(field, &self.defaults);
                let on_change = move |this: &mut Self,
                                      event: &InputEvent,
                                      window: &mut Window,
                                      cx: &mut Context<Self>| {
                    if matches!(event, InputEvent::Change) {
                        this.text_changed(path, window, cx);
                    }
                };
                let control = if field.kind.is_lines() {
                    let input = cx.new(|cx| {
                        let mut state = TextareaState::new(window, cx)
                            .auto_grow(2, 12)
                            .placeholder(placeholder);
                        state.set_value(value, window, cx);
                        state
                    });
                    self._subscriptions.push(cx.subscribe_in(
                        &input,
                        window,
                        move |this, _, event: &InputEvent, window, cx| {
                            on_change(this, event, window, cx)
                        },
                    ));
                    TextControl::Lines(input)
                } else {
                    let input = cx.new(|cx| {
                        let mut state = InputState::new(window, cx).placeholder(placeholder);
                        state.set_value(value, window, cx);
                        state
                    });
                    self._subscriptions.push(cx.subscribe_in(
                        &input,
                        window,
                        move |this, _, event: &InputEvent, window, cx| {
                            on_change(this, event, window, cx)
                        },
                    ));
                    TextControl::Line(input)
                };
                self.texts.insert(path, control);
            }
        }
        cx.notify();
    }

    #[cfg(test)]
    pub fn document(&self) -> Option<&ConfigDocument> {
        self.doc.as_ref().ok()
    }

    #[cfg(test)]
    pub fn error(&self, path: &str) -> Option<&String> {
        self.errors.get(path)
    }

    #[cfg(test)]
    pub fn show_section(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.section = ix;
        cx.notify();
    }

    #[cfg(test)]
    pub fn set_text(
        &mut self,
        path: &'static str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.texts.get(path) {
            Some(TextControl::Line(input)) => {
                input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx))
            }
            Some(TextControl::Lines(input)) => {
                input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx))
            }
            None => return,
        }
        self.text_changed(path, window, cx);
    }

    fn control_text(&self, path: &str, cx: &App) -> Option<String> {
        Some(match self.texts.get(path)? {
            TextControl::Line(input) => input.read(cx).value().to_string(),
            TextControl::Lines(input) => input.read(cx).value().to_string(),
        })
    }

    fn text_changed(&mut self, path: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(field), Some(text)) = (find_field(path), self.control_text(path, cx)) else {
            return;
        };
        let parsed = settings::parse_text(field.kind, &text);
        let keep = parsed.error.is_some() && parsed.value.is_none();
        match parsed.error {
            Some(error) => self.errors.insert(path, error),
            None => self.errors.remove(path),
        };
        if keep {
            cx.notify();
        } else {
            self.set(path, parsed.value, window, cx);
        }
    }

    pub fn set(
        &mut self,
        path: &str,
        value: Option<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(doc) = &mut self.doc else {
            return;
        };
        if settings::apply(doc, &self.defaults, path, value) {
            let yaml = doc.to_yaml();
            self.comments = false;
            self.yaml
                .update(cx, |editor, cx| editor.set_value(yaml, window, cx));
        }
        cx.notify();
    }

    fn set_pair(
        &mut self,
        path: &'static str,
        key: &str,
        value: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(doc) = &self.doc else {
            return;
        };
        let mut pairs = settings::pairs(doc.get(path));
        match value {
            Some(value) => match pairs.iter_mut().find(|(k, _)| k == key) {
                Some(pair) => pair.1 = value,
                None => pairs.push((key.to_owned(), value)),
            },
            None => pairs.retain(|(k, _)| k != key),
        }
        self.set(path, Some(settings::pairs_value(&pairs)), window, cx);
    }

    fn rules(&self) -> Result<Vec<PolicyRule>, String> {
        let Ok(doc) = &self.doc else {
            return Ok(Vec::new());
        };
        settings::rules(settings::current(doc, &self.defaults, "policy.rules"))
    }

    fn change_rules(
        &mut self,
        change: impl FnOnce(&mut Vec<PolicyRule>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(mut rules) = self.rules() else {
            return;
        };
        change(&mut rules);
        self.set(
            "policy.rules",
            Some(settings::rules_value(&rules)),
            window,
            cx,
        );
    }

    fn edit_rule(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let rule = ix
            .and_then(|ix| self.rules().ok()?.get(ix).cloned())
            .unwrap_or(PolicyRule {
                tool: String::new(),
                action: loom_config::PolicyAction::Allow,
                command_prefix: Vec::new(),
            });
        let this = cx.entity().downgrade();
        form::open(
            if ix.is_some() { "Edit rule" } else { "Add rule" },
            None,
            vec![
                FormField::text("Tool")
                    .value(rule.tool.clone())
                    .placeholder("shell")
                    .help("The name of a tool, such as read, edit, write, or shell."),
                FormField::choice(
                    "Action",
                    POLICY_ACTIONS.iter().map(|a| a.to_string()).collect(),
                )
                .value(settings::action_name(rule.action)),
                FormField::lines("Command prefixes", 3)
                    .value(rule.command_prefix.join("\n"))
                    .placeholder("git status")
                    .help("Only for the shell tool. One prefix on each line. Empty matches all commands."),
            ],
            if ix.is_some() { "Save" } else { "Add" },
            move |values, done, window, cx| {
                let tool = values.text(0);
                if tool.is_empty() {
                    return done.fail("Give the name of a tool.", window, cx);
                }
                let Some(action) = settings::parse_action(&values.text(1)) else {
                    return done.fail("Select an action.", window, cx);
                };
                let command_prefix = values.lines(2);
                if !command_prefix.is_empty() && tool != "shell" {
                    return done.fail("Only the shell tool can have command prefixes.", window, cx);
                }
                let rule = PolicyRule {
                    tool,
                    action,
                    command_prefix,
                };
                let _ = this.update(cx, |form, cx| {
                    form.change_rules(
                        move |rules| match ix.filter(|ix| *ix < rules.len()) {
                            Some(ix) => rules[ix] = rule,
                            None => rules.push(rule),
                        },
                        window,
                        cx,
                    )
                });
                done.finish(Ok(()), window, cx);
            },
            window,
            cx,
        );
    }

    fn add_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let secrets: Vec<String> = self
            .link
            .read(cx)
            .config
            .secrets
            .iter()
            .map(|s| s.name.clone())
            .collect();
        if secrets.is_empty() {
            return crate::ui::notify::error(window, cx, "Add a secret on the Secrets page first.");
        }
        let taken: Vec<String> = self
            .doc
            .as_ref()
            .map(|doc| {
                settings::pairs(doc.get("environment.secrets"))
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect()
            })
            .unwrap_or_default();
        let this = cx.entity().downgrade();
        form::open(
            "Add secret variable",
            Some("The codespace sets the variable to the value of the secret. The model sees only the name.".to_owned()),
            vec![
                FormField::text("Variable").placeholder("NPM_TOKEN"),
                FormField::choice("Secret", secrets),
            ],
            "Add",
            move |values, done, window, cx| {
                let name = values.text(0);
                if !valid_variable_name(&name) {
                    return done.fail("Give a variable name without spaces and =.", window, cx);
                }
                if taken.contains(&name) {
                    return done.fail(format!("The preset sets {name} already."), window, cx);
                }
                let secret = values.text(1);
                let _ = this.update(cx, |form, cx| {
                    form.set_pair("environment.secrets", &name, Some(secret), window, cx)
                });
                done.finish(Ok(()), window, cx);
            },
            window,
            cx,
        );
    }

    fn dropdown(
        &self,
        id: SharedString,
        current: Option<String>,
        options: Vec<(Option<String>, String)>,
        pick: impl Fn(&mut Self, Option<String>, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = options
            .iter()
            .find(|(value, _)| *value == current)
            .map(|(_, label)| label.clone())
            .or_else(|| current.clone())
            .unwrap_or_else(|| "None".to_owned());
        let this = cx.entity().downgrade();
        let pick = std::rc::Rc::new(pick);
        Button::new(id)
            .outline()
            .xsmall()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                for (value, label) in &options {
                    let (this, pick, value) = (this.clone(), pick.clone(), value.clone());
                    menu = menu.item(
                        PopupMenuItem::new(label.clone())
                            .checked(value == current)
                            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                                let _ = this
                                    .update(cx, |form, cx| pick(form, value.clone(), window, cx));
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    fn render_choice(
        &self,
        field: &'static Field,
        doc: &ConfigDocument,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (options, optional) = match field.kind {
            Kind::Choice(options) => (options, false),
            Kind::OptionalChoice(options) => (options, true),
            _ => return div().into_any_element(),
        };
        let default = self.defaults.get(field.path).map(settings::scalar_text);
        let current = settings::current(doc, &self.defaults, field.path)
            .map(settings::scalar_text)
            .filter(|value| !value.is_empty());
        let mut items = Vec::new();
        if optional {
            items.push((None, "Default".to_owned()));
        }
        items.extend(options.iter().map(|option| {
            (
                Some(option.to_string()),
                option_label(option, default.as_deref()),
            )
        }));
        let path = field.path;
        self.dropdown(
            id("choice", path),
            current,
            items,
            move |form, value, window, cx| form.set(path, value.map(Value::from), window, cx),
            cx,
        )
    }

    fn render_accounts(&self, doc: &ConfigDocument, cx: &mut Context<Self>) -> AnyElement {
        let accounts: Vec<(String, String)> = self
            .link
            .read(cx)
            .config
            .accounts
            .iter()
            .map(|a| (a.provider.clone(), a.name.clone()))
            .collect();
        let chosen = settings::pairs(doc.get("llm.accounts"));
        let mut providers: Vec<String> = accounts
            .iter()
            .map(|(provider, _)| provider.clone())
            .chain(chosen.iter().map(|(provider, _)| provider.clone()))
            .collect();
        providers.sort();
        providers.dedup();
        if providers.is_empty() {
            return detail(
                "The oracle has no model accounts. Add them on the Models page.",
                cx,
            )
            .into_any_element();
        }
        let mut list = v_flex().gap_1();
        for provider in providers {
            let current = chosen
                .iter()
                .find(|(p, _)| *p == provider)
                .map(|(_, account)| account.clone());
            let mut items = vec![(None, "None".to_owned())];
            items.extend(
                accounts
                    .iter()
                    .filter(|(p, _)| *p == provider)
                    .map(|(_, name)| (Some(name.clone()), name.clone())),
            );
            if let Some(current) = &current
                && !items
                    .iter()
                    .any(|(value, _)| value.as_ref() == Some(current))
            {
                items.push((
                    Some(current.clone()),
                    format!("{current} (not in the oracle)"),
                ));
            }
            let key = provider.clone();
            list = list.child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(160.)).text_sm().child(provider.clone()))
                    .child(self.dropdown(
                        id("account", &provider),
                        current,
                        items,
                        move |form, value, window, cx| {
                            form.set_pair("llm.accounts", &key, value, window, cx)
                        },
                        cx,
                    )),
            );
        }
        list.into_any_element()
    }

    fn render_credentials(&self, doc: &ConfigDocument, cx: &mut Context<Self>) -> AnyElement {
        let known: Vec<(String, String)> = self
            .link
            .read(cx)
            .config
            .credentials
            .iter()
            .map(|c| {
                let scope = match (&c.owner, &c.repository) {
                    (Some(owner), Some(repository)) => format!(", {owner}/{repository}"),
                    (Some(owner), None) => format!(", {owner}"),
                    _ => String::new(),
                };
                (
                    c.name.clone(),
                    format!("{} ({}{scope})", c.name, c.provider),
                )
            })
            .collect();
        let chosen = settings::strings(doc.get("git.credentials"));
        let mut names: Vec<String> = chosen.clone();
        names.extend(
            known
                .iter()
                .map(|(name, _)| name.clone())
                .filter(|name| !chosen.contains(name)),
        );
        if names.is_empty() {
            return detail(
                "The oracle has no Git credentials. Add them on the Git providers page.",
                cx,
            )
            .into_any_element();
        }
        let mut list = v_flex().gap_1();
        for (ix, name) in names.into_iter().enumerate() {
            let label = known
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| format!("{name} (not in the oracle)"));
            let checked = chosen.contains(&name);
            let chosen = chosen.clone();
            list = list.child(
                Checkbox::new(id("credential", ix))
                    .label(label)
                    .checked(checked)
                    .on_click(cx.listener(move |form, checked: &bool, window, cx| {
                        let mut list = chosen.clone();
                        list.retain(|n| *n != name);
                        if *checked {
                            list.push(name.clone());
                        }
                        form.set(
                            "git.credentials",
                            Some(settings::strings_value(&list)),
                            window,
                            cx,
                        );
                    })),
            );
        }
        list.into_any_element()
    }

    fn render_secrets(&self, doc: &ConfigDocument, cx: &mut Context<Self>) -> AnyElement {
        let secrets: Vec<String> = self
            .link
            .read(cx)
            .config
            .secrets
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let chosen = settings::pairs(doc.get("environment.secrets"));
        let mut list = v_flex().gap_1();
        for (ix, (variable, secret)) in chosen.into_iter().enumerate() {
            let mut items: Vec<(Option<String>, String)> = secrets
                .iter()
                .map(|s| (Some(s.clone()), s.clone()))
                .collect();
            if !secrets.contains(&secret) {
                items.push((
                    Some(secret.clone()),
                    format!("{secret} (not in the oracle)"),
                ));
            }
            let (pick, remove) = (variable.clone(), variable.clone());
            list =
                list.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .w(px(160.))
                                .text_sm()
                                .truncate()
                                .child(variable.clone()),
                        )
                        .child(self.dropdown(
                            id("secret", ix),
                            Some(secret),
                            items,
                            move |form, value, window, cx| {
                                if value.is_some() {
                                    form.set_pair("environment.secrets", &pick, value, window, cx)
                                }
                            },
                            cx,
                        ))
                        .child(small_button(id("remove-secret", ix), "Remove").on_click(
                            cx.listener(move |form, _, window, cx| {
                                form.set_pair("environment.secrets", &remove, None, window, cx)
                            }),
                        )),
                );
        }
        list.child(
            h_flex().child(
                small_button("agent-add-secret", "Add secret variable…")
                    .on_click(cx.listener(|form, _, window, cx| form.add_secret(window, cx))),
            ),
        )
        .into_any_element()
    }

    fn render_rules(&self, cx: &mut Context<Self>) -> AnyElement {
        let rules = match self.rules() {
            Ok(rules) => rules,
            Err(error) => {
                return danger(
                    format!("policy.rules is not valid: {error}. Correct it in the YAML view."),
                    cx,
                )
                .into_any_element();
            }
        };
        let mut list = v_flex().gap_1();
        if rules.is_empty() {
            list = list.child(detail("No rules.", cx));
        }
        let count = rules.len();
        for (ix, rule) in rules.iter().enumerate() {
            list =
                list.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .whitespace_normal()
                                .child(settings::rule_label(rule)),
                        )
                        .child(
                            small_button(id("rule-up", ix), "Up")
                                .disabled(ix == 0)
                                .on_click(cx.listener(move |form, _, window, cx| {
                                    form.change_rules(
                                        |rules| {
                                            if ix > 0 && ix < rules.len() {
                                                rules.swap(ix - 1, ix)
                                            }
                                        },
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            small_button(id("rule-down", ix), "Down")
                                .disabled(ix + 1 == count)
                                .on_click(cx.listener(move |form, _, window, cx| {
                                    form.change_rules(
                                        |rules| {
                                            if ix + 1 < rules.len() {
                                                rules.swap(ix, ix + 1)
                                            }
                                        },
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            small_button(id("rule-edit", ix), "Edit…").on_click(cx.listener(
                                move |form, _, window, cx| form.edit_rule(Some(ix), window, cx),
                            )),
                        )
                        .child(small_button(id("rule-remove", ix), "Remove").on_click(
                            cx.listener(move |form, _, window, cx| {
                                form.change_rules(
                                    |rules| {
                                        if ix < rules.len() {
                                            rules.remove(ix);
                                        }
                                    },
                                    window,
                                    cx,
                                )
                            }),
                        )),
                );
        }
        list.child(
            h_flex().child(
                small_button("agent-add-rule", "Add rule…")
                    .on_click(cx.listener(|form, _, window, cx| form.edit_rule(None, window, cx))),
            ),
        )
        .into_any_element()
    }

    fn render_field(
        &self,
        field: &'static Field,
        doc: &ConfigDocument,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path = field.path;
        let check = field.kind == Kind::Check;
        let control = match field.kind {
            Kind::Check => {
                let checked = settings::current(doc, &self.defaults, path)
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Checkbox::new(id("check", path))
                    .label(field.label)
                    .checked(checked)
                    .on_click(cx.listener(move |form, checked: &bool, window, cx| {
                        form.set(path, Some(Value::Bool(*checked)), window, cx)
                    }))
                    .into_any_element()
            }
            Kind::Choice(_) | Kind::OptionalChoice(_) => self.render_choice(field, doc, cx),
            Kind::Accounts => self.render_accounts(doc, cx),
            Kind::Credentials => self.render_credentials(doc, cx),
            Kind::Secrets => self.render_secrets(doc, cx),
            Kind::Rules => self.render_rules(cx),
            _ => match self.texts.get(path) {
                Some(TextControl::Line(input)) => Input::new(input).small().into_any_element(),
                Some(TextControl::Lines(input)) => Textarea::new(input).into_any_element(),
                None => div().into_any_element(),
            },
        };
        let control = if matches!(field.kind, Kind::Choice(_) | Kind::OptionalChoice(_)) {
            h_flex().child(control).into_any_element()
        } else {
            control
        };
        v_flex()
            .gap_1()
            .when(!check, |el| el.child(div().text_sm().child(field.label)))
            .child(control)
            .when_some(self.errors.get(path).cloned(), |el, error| {
                el.child(danger(error, cx))
            })
            .when(!field.help.is_empty(), |el| {
                el.child(detail(field.help, cx))
            })
            .into_any_element()
    }
}

impl Render for AgentForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .w_full()
            .debug_selector(|| "settings-agent-form".into());
        let doc = match &self.doc {
            Ok(doc) => doc.clone(),
            Err(_) => {
                return root
                    .child(warning(
                        "The form cannot read agent.yaml. Correct the file in the YAML view.",
                        cx,
                    ))
                    .into_any_element();
            }
        };
        let theme = cx.theme().clone();
        let mut nav = v_flex()
            .w(px(170.))
            .flex_none()
            .gap_0p5()
            .pr_2()
            .border_r_1()
            .border_color(theme.border);
        for (ix, section) in SECTIONS.iter().enumerate() {
            nav = nav.child(
                Button::new(id("section", section.id))
                    .ghost()
                    .xsmall()
                    .w_full()
                    .justify_start()
                    .label(section.title)
                    .selected(ix == self.section)
                    .on_click(cx.listener(move |form, _, _, cx| {
                        form.section = ix;
                        cx.notify();
                    })),
            );
        }
        let section = &SECTIONS[self.section.min(SECTIONS.len() - 1)];
        let mut fields = v_flex()
            .flex_1()
            .min_w_0()
            .gap_3()
            .child(detail(section.help, cx));
        for field in section.fields {
            fields = fields.child(self.render_field(field, &doc, cx));
        }
        root.child(
            v_flex()
                .gap_2()
                .when(self.comments, |el| {
                    el.child(warning(
                        "agent.yaml has comments. A change in the form writes the file again without the comments.",
                        cx,
                    ))
                })
                .child(h_flex().items_start().gap_3().child(nav).child(fields)),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_component::Root;
    use loom_client::oracle_proto::Secret;
    use serde_json::json;

    use super::*;
    use crate::oracle::tests::{TestOracle, wait};

    #[gpui::test]
    fn the_form_names_the_secrets_and_the_rules(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let test = TestOracle::start(&root);
        test.oracle
            .set_secret("npm-work", &Secret::new("token"), None)
            .unwrap();
        cx.update(gpui_component::init);
        let (entry, env, backend) = (test.entry.clone(), test.env.clone(), test.backend.clone());
        let link = cx.new(|cx| OracleLink::new(entry, env, backend, cx));
        wait(cx, |cx| !link.read(cx).config.secrets.is_empty());
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let (slot, link) = (slot.clone(), link.clone());
            move |window, cx| {
                let yaml = cx.new(|cx| {
                    let mut editor = EditorState::new(window, cx);
                    editor.set_value("policy:\n  default: ask\n", window, cx);
                    editor
                });
                let form = cx.new(|cx| AgentForm::new(link, yaml.clone(), window, cx));
                *slot.borrow_mut() = Some((form.clone(), yaml));
                Root::new(form, window, cx)
            }
        });
        let (form, yaml) = slot.borrow_mut().take().unwrap();
        let text =
            |cx: &mut gpui::VisualTestContext| yaml.read_with(cx, |e, _| e.value().to_string());

        form.update_in(cx, |form, window, cx| {
            form.set_pair(
                "environment.secrets",
                "NPM_TOKEN",
                Some("npm-work".into()),
                window,
                cx,
            );
            form.change_rules(
                |rules| {
                    rules.push(PolicyRule {
                        tool: "shell".into(),
                        action: loom_config::PolicyAction::Deny,
                        command_prefix: vec!["sudo".into()],
                    })
                },
                window,
                cx,
            );
        });
        for ix in 0..SECTIONS.len() {
            form.update(cx, |form, cx| form.show_section(ix, cx));
            cx.run_until_parked();
        }
        let doc = ConfigDocument::parse(&text(cx), "agent.yaml").unwrap();
        assert_eq!(
            doc.get("environment.secrets"),
            Some(&json!({"NPM_TOKEN": "npm-work"}))
        );
        assert_eq!(
            doc.get("policy.rules"),
            Some(&json!([
                {"tool": "read", "action": "allow"},
                {"tool": "shell", "action": "deny", "command_prefix": ["sudo"]},
            ]))
        );
        assert_eq!(doc.get("policy.default"), Some(&json!("ask")));

        form.update_in(cx, |form, window, cx| {
            form.change_rules(|rules| rules.truncate(1), window, cx);
            form.set_pair("environment.secrets", "NPM_TOKEN", None, window, cx);
        });
        assert_eq!(text(cx), "policy:\n  default: ask\n");

        yaml.update_in(cx, |editor, window, cx| {
            editor.set_value("policy: [", window, cx)
        });
        form.update_in(cx, |form, window, cx| form.reload(window, cx));
        cx.run_until_parked();
        assert!(form.read_with(cx, |form, _| form.document().is_none()));
        assert!(cx.debug_bounds("settings-agent-form").is_some());
    }
}
