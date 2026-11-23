use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{
    App, AppContext, AsyncWindowContext, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use loom_client::OracleClient;
use loom_client::oracle_proto::{
    CodespaceState, FirstMessage, Host, Models, Preset, Repository, Workspace,
};
use loom_view_model::codespaces::{
    NewCodespaceDefaults, directory, format_memory, parse_memory, state_label, title,
};

use crate::backend::Backend;
use crate::oracle::OracleLink;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCodespaceRequest {
    pub workspace: String,
    pub preset: Option<String>,
    pub cpus: u32,
    pub memory_mib: u64,
    pub base_branches: BTreeMap<String, String>,
    pub first_message: Option<FirstMessage>,
}

pub type OnCreate = Rc<dyn Fn(NewCodespaceRequest, &mut Window, &mut App)>;
pub type OnOpen = Rc<dyn Fn(&mut Window, &mut App)>;

pub struct NewCodespaceForm {
    pub oracle: String,
    pub workspace: Workspace,
    pub presets: Vec<Preset>,
    pub host: Option<Host>,
    pub defaults: NewCodespaceDefaults,
}

pub struct NewCodespaceView {
    form: NewCodespaceForm,
    preset: Option<String>,
    cpus: Entity<InputState>,
    memory: Entity<InputState>,
    branches: Vec<(String, Option<String>, Entity<InputState>)>,
    message: Entity<TextareaState>,
    model: Option<String>,
    models: Option<Models>,
    warnings: Vec<String>,
    error: Option<String>,
    client: OracleClient,
    backend: Backend,
    on_create: OnCreate,
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl NewCodespaceView {
    fn choose_preset(
        &mut self,
        preset: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preset = preset;
        self.model = None;
        self.models = None;
        self.warnings.clear();
        self.generation += 1;
        let generation = self.generation;
        let client = self.client.clone();
        let workspace = self.form.workspace.name.clone();
        let preset = self.preset.clone();
        let task = self.backend.run(async move {
            let warnings = client
                .codespace_warnings(&workspace, preset.as_deref())
                .await
                .unwrap_or_default();
            let models = match &preset {
                Some(preset) => client.models(preset).await.ok(),
                None => None,
            };
            (warnings, models)
        });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some((warnings, models)) = result {
                    this.warnings = warnings;
                    this.model = models.as_ref().and_then(|m| m.default_model.clone());
                    this.models = models;
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cpus = self.cpus.read(cx).value().trim().to_owned();
        let Some(cpus) = cpus.parse::<u32>().ok().filter(|n| *n > 0) else {
            self.error = Some(format!("{cpus:?} is not a number of CPUs."));
            cx.notify();
            return;
        };
        let memory = self.memory.read(cx).value().trim().to_owned();
        let Some(memory_mib) = parse_memory(&memory) else {
            self.error = Some(format!(
                "{memory:?} is not an amount of memory, such as 4 GiB or 512 MiB."
            ));
            cx.notify();
            return;
        };
        let base_branches = self
            .branches
            .iter()
            .filter_map(|(directory, _, input)| {
                let branch = input.read(cx).value().trim().to_owned();
                (!branch.is_empty()).then(|| (directory.clone(), branch))
            })
            .collect();
        let text = self.message.read(cx).value().trim().to_owned();
        let first_message = (!text.is_empty()).then(|| FirstMessage {
            text,
            model: self.model.clone(),
        });
        let request = NewCodespaceRequest {
            workspace: self.form.workspace.name.clone(),
            preset: self.preset.clone(),
            cpus,
            memory_mib,
            base_branches,
            first_message,
        };
        window.close_dialog(cx);
        (self.on_create)(request, window, cx);
    }

    fn preset_info(&self) -> Option<&Preset> {
        let name = self.preset.as_deref()?;
        self.form.presets.iter().find(|p| p.name == name)
    }

    fn render_preset_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let names: Vec<String> = self.form.presets.iter().map(|p| p.name.clone()).collect();
        let current = self.preset.clone();
        let label = current.clone().unwrap_or_else(|| "No preset".to_owned());
        let this = cx.entity().downgrade();
        Button::new("codespace-preset")
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let pick = |preset: Option<String>| {
                    let this = this.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |view, cx| {
                            view.choose_preset(preset.clone(), window, cx)
                        });
                    }
                };
                for name in &names {
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(current.as_deref() == Some(name))
                            .on_click(pick(Some(name.clone()))),
                    );
                }
                menu.separator().item(
                    PopupMenuItem::new("No preset")
                        .checked(current.is_none())
                        .on_click(pick(None)),
                )
            })
    }

    fn render_model_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let models: Vec<(String, String)> = self
            .models
            .as_ref()
            .map(|m| {
                m.models
                    .iter()
                    .map(|model| {
                        let id = model.id.clone();
                        let name = model.display_name.clone().unwrap_or_else(|| id.clone());
                        let label = match &model.account {
                            Some(account) => format!("{name} ({}, {account})", model.provider),
                            None => format!("{name} ({})", model.provider),
                        };
                        (id, label)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let current = self.model.clone();
        let label = current
            .as_ref()
            .and_then(|id| models.iter().find(|(m, _)| m == id).map(|(_, l)| l.clone()))
            .or_else(|| current.clone())
            .unwrap_or_else(|| "Default model of the agent".to_owned());
        let this = cx.entity().downgrade();
        Button::new("codespace-model")
            .outline()
            .small()
            .label(label)
            .dropdown_menu(move |mut menu, _, _| {
                let pick = |model: Option<String>| {
                    let this = this.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ = this.update(cx, |view, cx| {
                            view.model = model.clone();
                            cx.notify();
                        });
                    }
                };
                for (id, label) in &models {
                    menu = menu.item(
                        PopupMenuItem::new(label.clone())
                            .checked(current.as_deref() == Some(id))
                            .on_click(pick(Some(id.clone()))),
                    );
                }
                menu.separator().item(
                    PopupMenuItem::new("Default model of the agent")
                        .checked(current.is_none())
                        .on_click(pick(None)),
                )
            })
    }
}

impl Render for NewCodespaceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let label = |text: &'static str| div().text_sm().child(text);
        let note = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(text)
        };
        let host = match &self.form.host {
            Some(host) => {
                let state = if host.connected {
                    "connected"
                } else {
                    "not connected"
                };
                match &host.report {
                    Some(report) => format!(
                        "The codespace runs on the host {} ({state}, {} CPUs, {}).",
                        host.name,
                        report.cpus,
                        format_memory(report.memory_mib)
                    ),
                    None => format!("The codespace runs on the host {} ({state}).", host.name),
                }
            }
            None => format!(
                "The codespace runs on the host {}.",
                self.form.workspace.host
            ),
        };
        let accounts = self.preset_info().map(|preset| {
            if preset.model_accounts.is_empty() {
                "The preset names no model accounts.".to_owned()
            } else {
                let accounts: Vec<String> = preset
                    .model_accounts
                    .iter()
                    .map(|(provider, account)| format!("{provider}: {account}"))
                    .collect();
                format!("Model accounts: {}.", accounts.join(", "))
            }
        });
        let model_errors = self
            .models
            .as_ref()
            .map(|m| m.errors.clone())
            .unwrap_or_default();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(format!(
                        "Start a codespace for the workspace {} of {}. {host}",
                        self.form.workspace.name, self.form.oracle
                    )),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(label("Preset"))
                    .child(h_flex().child(self.render_preset_menu(cx)))
                    .when_some(accounts, |el, text| el.child(note(text))),
            )
            .children(self.warnings.iter().map(|warning| {
                div()
                    .text_xs()
                    .text_color(theme.warning)
                    .whitespace_normal()
                    .child(SharedString::from(format!("Warning: {warning}.")))
            }))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(label("CPUs"))
                            .child(Input::new(&self.cpus)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(label("Memory"))
                            .child(Input::new(&self.memory)),
                    ),
            )
            .child(v_flex().gap_1().child(label("Base branches")).children(
                self.branches.iter().map(|(directory, _, input)| {
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .w(px(140.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_sm()
                                .child(directory.clone()),
                        )
                        .child(div().flex_1().child(Input::new(input)))
                }),
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(label("First message (optional)"))
                    .child(Textarea::new(&self.message))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().child("Model"))
                            .child(self.render_model_menu(cx)),
                    )
                    .children(
                        model_errors
                            .into_iter()
                            .map(|error| note(format!("Models: {error}"))),
                    ),
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
                    Button::new("create-codespace")
                        .primary()
                        .label("Create")
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
            )
    }
}

fn branch_placeholder(repository: &Repository) -> String {
    match &repository.base_branch {
        Some(branch) => branch.clone(),
        None => "the default branch".to_owned(),
    }
}

pub fn open(
    form: NewCodespaceForm,
    client: OracleClient,
    backend: Backend,
    on_create: OnCreate,
    window: &mut Window,
    cx: &mut App,
) -> Entity<NewCodespaceView> {
    let view = cx.new(|cx| {
        let cpus = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("2");
            state.set_value(form.defaults.cpus.to_string(), window, cx);
            state
        });
        let memory = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("4 GiB");
            state.set_value(format_memory(form.defaults.memory_mib), window, cx);
            state
        });
        let branches = form
            .workspace
            .repositories
            .iter()
            .map(|repository| {
                let placeholder = branch_placeholder(repository);
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                (directory(repository), repository.base_branch.clone(), input)
            })
            .collect();
        let message = cx.new(|cx| {
            TextareaState::new(window, cx).auto_grow(3, 10).placeholder(
                "The agent starts a session with this message when the codespace is ready.",
            )
        });
        let preset = form
            .defaults
            .preset
            .clone()
            .filter(|name| form.presets.iter().any(|p| &p.name == name))
            .or_else(|| form.presets.first().map(|p| p.name.clone()));
        let mut view = NewCodespaceView {
            form,
            preset: None,
            cpus,
            memory,
            branches,
            message,
            model: None,
            models: None,
            warnings: Vec::new(),
            error: None,
            client,
            backend,
            on_create,
            generation: 0,
            _subscriptions: Vec::new(),
        };
        view.choose_preset(preset, window, cx);
        view
    });
    let dialog_view = view.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("New codespace")
            .w(px(600.))
            .child(dialog_view.clone())
    });
    view
}

pub struct ProgressView {
    link: Entity<OracleLink>,
    codespace: String,
    on_open: OnOpen,
    _subscription: Subscription,
}

impl Render for ProgressView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let link = self.link.read(cx);
        let codespace = link.codespace(&self.codespace).cloned();
        let steps = link
            .progress
            .get(&self.codespace)
            .cloned()
            .unwrap_or_default();
        let Some(codespace) = codespace else {
            return v_flex()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("The oracle no longer lists this codespace.")
                .into_any_element();
        };
        let ready = codespace.state == CodespaceState::Ready;
        let failed = codespace.state == CodespaceState::Failed;
        let color = if failed {
            theme.danger
        } else if ready {
            theme.success
        } else {
            theme.info
        };
        let on_open = self.on_open.clone();
        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title(&codespace)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(color)
                    .whitespace_normal()
                    .child(state_label(&codespace)),
            )
            .child(
                v_flex()
                    .id("codespace-steps")
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .gap_0p5()
                    .p_2()
                    .rounded_md()
                    .bg(theme.muted)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .when(steps.is_empty(), |el| {
                        el.child(div().text_color(theme.muted_foreground).child(
                            "No steps yet. The oracle sends them while the codespace starts.",
                        ))
                    })
                    .children(
                        steps
                            .into_iter()
                            .map(|step| div().whitespace_normal().child(step)),
                    ),
            )
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("open-codespace")
                        .primary()
                        .label("Open")
                        .disabled(!ready)
                        .on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            on_open(window, cx);
                        }),
                ),
            )
            .into_any_element()
    }
}

pub fn open_progress(
    link: Entity<OracleLink>,
    codespace: String,
    on_open: OnOpen,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| {
        let subscription = cx.observe(&link, |_, _, cx| cx.notify());
        ProgressView {
            link,
            codespace,
            on_open,
            _subscription: subscription,
        }
    });
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Codespace start")
            .w(px(600.))
            .child(view.clone())
    });
}
