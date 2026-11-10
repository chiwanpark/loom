use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, AppContext, AsyncWindowContext, Context, Entity, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Sizable, WindowExt, h_flex, v_flex};
use loom_client::BrowserCallback;
use loom_client::proto::{
    LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt, ModelInfo, ProviderStatus, SessionId,
};

use crate::state::{AgentConnection, ConnectionEvent};
use crate::ui::notify;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

pub type Submit = Rc<dyn Fn(Vec<String>, &mut Window, &mut App)>;

pub struct FormField {
    pub label: &'static str,
    pub placeholder: &'static str,
    pub value: String,
    pub masked: bool,
}

impl FormField {
    pub fn new(label: &'static str, placeholder: &'static str) -> Self {
        Self {
            label,
            placeholder,
            value: String::new(),
            masked: false,
        }
    }

    pub fn value(mut self, value: impl Into<String>) -> Self {
        self.value = value.into();
        self
    }

    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }
}

pub struct FormView {
    description: Option<String>,
    fields: Vec<(&'static str, Entity<InputState>)>,
    submit_label: &'static str,
    submit: Submit,
    _subscriptions: Vec<Subscription>,
}

impl FormView {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let values: Vec<String> = self
            .fields
            .iter()
            .map(|(_, input)| input.read(cx).value().trim().to_owned())
            .collect();
        window.close_dialog(cx);
        (self.submit)(values, window, cx);
    }
}

impl Render for FormView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .gap_3()
            .when_some(self.description.clone(), |el, text| {
                el.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(text),
                )
            })
            .children(self.fields.iter().map(|(label, input)| {
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(*label))
                    .child(Input::new(input))
            }))
            .child(
                h_flex().justify_end().gap_2().child(
                    Button::new("submit")
                        .primary()
                        .label(self.submit_label)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
            )
    }
}

pub fn open_form(
    title: &'static str,
    description: Option<String>,
    fields: Vec<FormField>,
    submit_label: &'static str,
    submit: impl Fn(Vec<String>, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| {
        let mut subscriptions = Vec::new();
        let inputs: Vec<(&'static str, Entity<InputState>)> = fields
            .into_iter()
            .map(|field| {
                let input = cx.new(|cx| {
                    let mut state = InputState::new(window, cx).placeholder(field.placeholder);
                    if field.masked {
                        state = state.masked(true);
                    }
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
                (field.label, input)
            })
            .collect();
        FormView {
            description,
            fields: inputs,
            submit_label,
            submit: Rc::new(submit),
            _subscriptions: subscriptions,
        }
    });
    let first = view.read(cx).fields.first().map(|(_, input)| input.clone());
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title).w(px(520.)).child(view.clone())
    });
    focus_later(first, window, cx);
}

fn focus_later(input: Option<Entity<InputState>>, window: &mut Window, cx: &mut App) {
    if let Some(input) = input {
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| input.focus(window, cx));
        });
    }
}

pub struct ModelPicker {
    connection: Entity<AgentConnection>,
    session: Option<SessionId>,
    filter: Entity<InputState>,
    models: Vec<ModelInfo>,
    errors: Vec<String>,
    loading: bool,
    _subscriptions: Vec<Subscription>,
}

impl ModelPicker {
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let task = conn.backend.run(async move {
            let providers = client.list_providers().await?;
            let mut models = Vec::new();
            let mut errors = Vec::new();
            for provider in providers.iter().filter(|p| p.enabled) {
                match client.list_models(provider.provider.as_str()).await {
                    Ok(list) => models.extend(list),
                    Err(err) if provider.signed_in() => {
                        errors.push(format!("{}: {err}", provider.display_name))
                    }
                    Err(_) => {}
                }
            }
            Ok::<_, loom_client::ClientError>((models, errors))
        });
        self.loading = true;
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Some(Ok((models, errors))) => {
                        this.models = models;
                        this.errors = errors;
                    }
                    Some(Err(err)) => this.errors = vec![err.to_string()],
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose(&mut self, model: String, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let Some(session) = self.session.clone() else {
            return;
        };
        let connection = self.connection.clone();
        let conn = connection.read(cx);
        let client = conn.client.clone();
        let task = conn
            .backend
            .run(async move { client.set_model(&session, Some(&model)).await });
        cx.spawn_in(
            window,
            async move |_, cx: &mut AsyncWindowContext| match task.await {
                Some(Ok(info)) => {
                    connection.update(cx, |conn, cx| conn.update_session_info(info, cx));
                }
                Some(Err(err)) => {
                    let _ = cx.update(|window, cx| notify::error(window, cx, err.to_string()));
                }
                None => {}
            },
        )
        .detach();
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let query = self.filter.read(cx).value().to_lowercase();
        let models: Vec<ModelInfo> = self
            .models
            .iter()
            .filter(|m| query.is_empty() || m.model_ref().to_lowercase().contains(&query))
            .cloned()
            .collect();
        v_flex()
            .gap_2()
            .child(Input::new(&self.filter))
            .when(self.loading, |el| {
                el.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Loading models…"),
                )
            })
            .children(self.errors.iter().map(|e| {
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .whitespace_normal()
                    .child(e.clone())
            }))
            .when(
                !self.loading && self.models.is_empty() && self.errors.is_empty(),
                |el| {
                    el.child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("No models. Sign in to a provider first."),
                    )
                },
            )
            .child(
                v_flex()
                    .id("models")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .children(models.into_iter().enumerate().map(|(ix, model)| {
                        let model_ref = model.model_ref();
                        let chosen = model_ref.clone();
                        h_flex()
                            .id(("model", ix))
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_2()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.list_hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose(chosen.clone(), window, cx)
                            }))
                            .child(div().flex_1().child(model.display_name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(model_ref),
                            )
                    })),
            )
    }
}

pub fn open_model_picker(
    connection: Entity<AgentConnection>,
    session: Option<SessionId>,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter models"));
        let subscriptions = vec![cx.subscribe_in(
            &filter,
            window,
            |_: &mut ModelPicker, _, _: &InputEvent, _, cx| cx.notify(),
        )];
        let mut picker = ModelPicker {
            connection,
            session,
            filter,
            models: Vec::new(),
            errors: Vec::new(),
            loading: false,
            _subscriptions: subscriptions,
        };
        picker.load(window, cx);
        picker
    });
    let filter = view.read(cx).filter.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Choose a model")
            .w(px(560.))
            .child(view.clone())
    });
    focus_later(Some(filter), window, cx);
}

enum Pending {
    Browser {
        provider: String,
    },
    Device {
        provider: String,
        code: String,
        uri: String,
    },
}

pub struct ProvidersView {
    connection: Entity<AgentConnection>,
    providers: Vec<ProviderStatus>,
    pending: Option<Pending>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl ProvidersView {
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let task = conn
            .backend
            .run(async move { client.list_providers().await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Some(Ok(providers)) => this.providers = providers,
                    Some(Err(err)) => this.error = Some(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn browser_login(
        &mut self,
        provider: String,
        redirect: loom_client::proto::RedirectSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let backend = conn.backend.clone();
        let name = provider.clone();
        let start = backend.run(async move {
            let callback = BrowserCallback::bind(&redirect).await?;
            let started = client
                .start_login(
                    &name,
                    LoginMethod::Browser {
                        redirect_uri: Some(callback.redirect_uri().to_owned()),
                        account: None,
                    },
                )
                .await?;
            Ok::<_, loom_client::ClientError>((callback, started, client))
        });
        self.pending = Some(Pending::Browser {
            provider: provider.clone(),
        });
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let (callback, started, client) = match start.await {
                Some(Ok(parts)) => parts,
                Some(Err(err)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.pending = None;
                        this.error = Some(err.to_string());
                        cx.notify();
                    });
                    return;
                }
                None => return,
            };
            let LoginPrompt::OpenUrl { url, .. } = &started.prompt else {
                return;
            };
            let _ = cx.update(|_, cx| cx.open_url(url));
            let login = started.login_id.clone();
            let finish = backend.run(async move {
                let redirect = callback.wait(LOGIN_TIMEOUT).await;
                match redirect {
                    Ok(url) => {
                        client
                            .complete_login(&login, LoginInput::Redirect { url })
                            .await
                    }
                    Err(err) => {
                        let _ = client.cancel_login(&login).await;
                        Err(err)
                    }
                }
            });
            let result = finish.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pending = None;
                match result {
                    Some(Ok(account)) => {
                        let who = account
                            .label
                            .or(account.account_id)
                            .unwrap_or_else(|| account.provider.to_string());
                        notify::success(window, cx, format!("Signed in as {who}"));
                    }
                    Some(Err(err)) => this.error = Some(err.to_string()),
                    None => {}
                }
                this.load(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn device_login(&mut self, provider: String, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let name = provider.clone();
        let task = conn
            .backend
            .run(async move { client.start_login(&name, LoginMethod::DeviceCode).await });
        self.error = None;
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Some(Ok(started)) => {
                        if let LoginPrompt::DeviceCode {
                            user_code,
                            verification_uri,
                            ..
                        } = started.prompt
                        {
                            cx.open_url(&verification_uri);
                            this.pending = Some(Pending::Device {
                                provider,
                                code: user_code,
                                uri: verification_uri,
                            });
                        }
                    }
                    Some(Err(err)) => this.error = Some(err.to_string()),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_api_key(&mut self, provider: String, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        open_form(
            "Set API key",
            Some(format!("The key is stored by the agent for {provider}.")),
            vec![FormField::new("API key", "sk-…").masked()],
            "Save",
            move |values, window, cx| {
                let key = values.into_iter().next().unwrap_or_default();
                if key.is_empty() {
                    return;
                }
                this.update(cx, |view, cx| {
                    let conn = view.connection.read(cx);
                    let client = conn.client.clone();
                    let provider = provider.clone();
                    let task = conn
                        .backend
                        .run(async move { client.set_api_key(&provider, Some(key)).await });
                    cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
                        let result = task.await;
                        let _ = this.update_in(cx, |this, window, cx| {
                            if let Some(Err(err)) = result {
                                this.error = Some(err.to_string());
                            }
                            this.load(window, cx);
                        });
                    })
                    .detach();
                });
            },
            window,
            cx,
        );
    }

    fn logout(&mut self, provider: String, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.connection.read(cx);
        let client = conn.client.clone();
        let task = conn
            .backend
            .run(async move { client.logout(&provider).await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(Err(err)) = result {
                    this.error = Some(err.to_string());
                }
                this.load(window, cx);
            });
        })
        .detach();
    }
}

impl Render for ProvidersView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().gap_2();
        if let Some(pending) = &self.pending {
            let text = match pending {
                Pending::Browser { provider } => {
                    format!("Finish signing in to {provider} in your browser…")
                }
                Pending::Device {
                    provider,
                    code,
                    uri,
                } => format!("Enter the code {code} at {uri} to sign in to {provider}."),
            };
            list = list.child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .whitespace_normal()
                    .child(text),
            );
        }
        if let Some(error) = &self.error {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .whitespace_normal()
                    .child(error.clone()),
            );
        }
        for (ix, provider) in self.providers.iter().enumerate() {
            let id = provider.provider.to_string();
            let status = if !provider.enabled {
                "disabled".to_owned()
            } else if let Some(account) = provider.accounts.iter().find(|a| a.active && a.signed_in)
            {
                format!(
                    "signed in{}",
                    account
                        .label
                        .as_ref()
                        .map(|l| format!(" as {l}"))
                        .unwrap_or_default()
                )
            } else if provider.api_key_set {
                "API key set".into()
            } else {
                "not signed in".into()
            };
            let mut buttons = h_flex().gap_1();
            for (mix, method) in provider.login_methods.iter().enumerate() {
                let pid = id.clone();
                let button = match method {
                    LoginMethodInfo::Browser { redirect } => {
                        let redirect = redirect.clone();
                        Button::new(SharedString::from(format!("login-{ix}-{mix}")))
                            .small()
                            .label("Sign in")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.browser_login(pid.clone(), redirect.clone(), window, cx)
                            }))
                    }
                    LoginMethodInfo::DeviceCode => {
                        Button::new(SharedString::from(format!("login-{ix}-{mix}")))
                            .small()
                            .label("Device code")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.device_login(pid.clone(), window, cx)
                            }))
                    }
                    LoginMethodInfo::ApiKey => {
                        Button::new(SharedString::from(format!("login-{ix}-{mix}")))
                            .small()
                            .label("API key")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_api_key(pid.clone(), window, cx)
                            }))
                    }
                };
                buttons = buttons.child(button);
            }
            if provider.signed_in() {
                let pid = id.clone();
                buttons = buttons.child(
                    Button::new(SharedString::from(format!("logout-{ix}")))
                        .small()
                        .ghost()
                        .label("Sign out")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.logout(pid.clone(), window, cx)
                        })),
                );
            }
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(provider.display_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{id} · {status}")),
                            ),
                    )
                    .when(provider.enabled, |el| el.child(buttons)),
            );
        }
        list
    }
}

pub fn open_providers(connection: Entity<AgentConnection>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| {
        let subscriptions = vec![cx.subscribe_in(
            &connection,
            window,
            |this: &mut ProvidersView, _, event: &ConnectionEvent, window, cx| {
                if let ConnectionEvent::LoginFinished(result) = event {
                    if matches!(this.pending, Some(Pending::Device { .. })) {
                        this.pending = None;
                    }
                    match result {
                        Ok(who) => notify::success(window, cx, format!("Signed in as {who}")),
                        Err(err) => this.error = Some(err.message.clone()),
                    }
                    this.load(window, cx);
                }
            },
        )];
        let mut view = ProvidersView {
            connection: connection.clone(),
            providers: Vec::new(),
            pending: None,
            error: None,
            _subscriptions: subscriptions,
        };
        view.load(window, cx);
        view
    });
    let title = format!("Providers · {}", connection.read(cx).title());
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(px(620.)).child(view.clone())
    });
}
