use std::time::Duration;

use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::ButtonVariants;
use gpui_component::{ActiveTheme, h_flex, v_flex};
use loom_client::BrowserCallback;
use loom_client::oracle_proto::{
    AccountKind, ModelAccount, ModelLogin, ModelLoginInput, ModelLoginMethod, ModelLoginPrompt,
};
use loom_client::proto::RedirectSpec;
use loom_view_model::settings::{
    DEVICE_CODE_PROVIDERS, SIGN_IN_PROVIDERS, age, parse_redirect, secret_uses,
};

use super::form::{self, Field};
use super::{SettingsView, card, danger, detail, empty, header, now_unix, small_button};
use crate::ui::notify;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub struct PendingLogin {
    pub login: ModelLogin,
    pub catching: bool,
}

impl SettingsView {
    pub(super) fn login_finished(
        &mut self,
        login: &str,
        account: Option<&ModelAccount>,
        error: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.login.as_ref().is_none_or(|p| p.login.login != login) {
            return;
        }
        self.login = None;
        match (account, error) {
            (Some(account), _) => notify::success(
                window,
                cx,
                format!("Signed in the model account {}.", account.name),
            ),
            (None, error) => notify::error(
                window,
                cx,
                format!("The sign-in failed: {}", error.unwrap_or("no reason")),
            ),
        }
        cx.notify();
    }

    fn sign_in(
        &mut self,
        account: String,
        provider: String,
        device_code: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pending) = self.login.take() {
            let login = pending.login.login;
            let task = self.call(cx, move |client| async move {
                client.cancel_model_login(&login).await
            });
            self.act(None, task, |_, _, _, _| {}, window, cx);
        }
        let method = if device_code {
            ModelLoginMethod::DeviceCode
        } else {
            ModelLoginMethod::Browser { redirect_uri: None }
        };
        let task = self.call(cx, move |client| async move {
            client.start_model_login(&account, &provider, method).await
        });
        self.act(
            Some("login".to_owned()),
            task,
            |this, login, window, cx| this.show_login(login, window, cx),
            window,
            cx,
        );
    }

    fn show_login(&mut self, login: ModelLogin, window: &mut Window, cx: &mut Context<Self>) {
        let mut catching = false;
        match &login.prompt {
            ModelLoginPrompt::OpenUrl { url, redirect_uri } => {
                cx.open_url(url);
                if let Some(redirect) = parse_redirect(redirect_uri) {
                    catching = true;
                    self.catch_redirect(login.login.clone(), redirect, window, cx);
                }
            }
            ModelLoginPrompt::DeviceCode {
                verification_uri, ..
            } => cx.open_url(verification_uri),
            ModelLoginPrompt::Unknown => {}
        }
        self.login = Some(PendingLogin { login, catching });
        cx.notify();
    }

    fn catch_redirect(
        &mut self,
        login: String,
        redirect: loom_view_model::settings::Redirect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec = RedirectSpec {
            host: redirect.host,
            port: Some(redirect.port),
            path: redirect.path,
        };
        let link = self.link.read(cx);
        let Some(client) = link.client() else {
            return;
        };
        let task = link.backend().run(async move {
            let callback = BrowserCallback::bind(&spec)
                .await
                .map_err(|err| format!("cannot catch the browser redirect: {err}"))?;
            let url = callback
                .wait(CALLBACK_TIMEOUT)
                .await
                .map_err(|err| err.to_string())?;
            client
                .complete_model_login(&login, ModelLoginInput::Redirect { url })
                .await
                .map_err(|err| err.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(pending) = &mut this.login {
                    pending.catching = false;
                }
                match result {
                    Some(Ok(account)) => {
                        this.login = None;
                        notify::success(
                            window,
                            cx,
                            format!("Signed in the model account {}.", account.name),
                        );
                    }
                    Some(Err(error)) if this.login.is_some() => notify::info(
                        window,
                        cx,
                        format!("{error}. Paste the URL from the browser."),
                    ),
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn paste_login(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = &self.login else {
            return;
        };
        let login = pending.login.login.clone();
        let link = self.link.clone();
        let this = cx.entity().downgrade();
        form::open(
            "Finish the sign-in",
            Some("After you sign in, the browser shows a code or goes to a URL. Paste the code or the full URL.".to_owned()),
            vec![Field::text("Code or URL")],
            "Finish",
            move |values, done, window, cx| {
                let text = values.text(0);
                if text.is_empty() {
                    return done.fail("Paste the code or the URL.", window, cx);
                }
                let input = if text.starts_with("http://") || text.starts_with("https://") {
                    ModelLoginInput::Redirect { url: text }
                } else {
                    ModelLoginInput::Code { code: text }
                };
                let login = login.clone();
                let task = super::call(&link, cx, move |client| async move {
                    client.complete_model_login(&login, input).await
                });
                let this = this.clone();
                done.wait(
                    task,
                    move |account, window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.login = None;
                            cx.notify();
                        });
                        notify::success(
                            window,
                            cx,
                            format!("Signed in the model account {}.", account.name),
                        );
                    },
                    window,
                    cx,
                );
            },
            window,
            cx,
        );
    }

    fn cancel_login(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.login.take() else {
            return;
        };
        let login = pending.login.login;
        let task = self.call(cx, move |client| async move {
            client.cancel_model_login(&login).await
        });
        self.act(None, task, |_, _, _, _| {}, window, cx);
    }

    fn add_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let link = self.link.clone();
        let mut methods: Vec<String> = Vec::new();
        for provider in SIGN_IN_PROVIDERS {
            methods.push(format!("{provider}: sign in"));
            if DEVICE_CODE_PROVIDERS.contains(provider) {
                methods.push(format!("{provider}: sign in with a device code"));
            }
        }
        for provider in DEVICE_CODE_PROVIDERS {
            if !SIGN_IN_PROVIDERS.contains(provider) {
                methods.push(format!("{provider}: sign in with a device code"));
            }
        }
        methods.push("API key".to_owned());
        form::open(
            "Add model account",
            Some("The oracle keeps the credentials and refreshes the tokens. Codespaces get short-lived tokens only.".to_owned()),
            vec![
                Field::text("Name").placeholder("anthropic-work"),
                Field::choice("Method", methods),
                Field::text("Provider")
                    .placeholder("openrouter")
                    .help("Only for an API key: the provider of the key, for example openai, opencode-go, or a provider from llm.providers of a preset."),
                Field::secret("API key").help("Only for an API key."),
            ],
            "Add",
            move |values, done, window, cx| {
                let name = values.text(0);
                if name.is_empty() {
                    return done.fail("Give a name.", window, cx);
                }
                let method = values.text(1);
                if method == "API key" {
                    let provider = values.text(2);
                    let key = values.text(3);
                    if provider.is_empty() || key.is_empty() {
                        return done.fail("Give the provider and the API key.", window, cx);
                    }
                    let task = super::call(&link, cx, move |client| async move {
                        client.set_model_api_key(&name, &provider, &key).await
                    });
                    return done.wait(task, |_, _, _| {}, window, cx);
                }
                let Some((provider, how)) = method.split_once(": ") else {
                    return done.fail("Select a method.", window, cx);
                };
                let device_code = how.contains("device code");
                let provider = provider.to_owned();
                done.finish(Ok(()), window, cx);
                let _ = this.update(cx, |view, cx| {
                    view.sign_in(name, provider, device_code, window, cx)
                });
            },
            window,
            cx,
        );
    }

    fn replace_key(&mut self, account: &ModelAccount, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.clone();
        let (name, provider) = (account.name.clone(), account.provider.clone());
        form::open(
            format!("Replace the API key of {name}"),
            None,
            vec![Field::secret("API key")],
            "Save",
            move |values, done, window, cx| {
                let key = values.text(0);
                if key.is_empty() {
                    return done.fail("Give the API key.", window, cx);
                }
                let (name, provider) = (name.clone(), provider.clone());
                let task = super::call(&link, cx, move |client| async move {
                    client.set_model_api_key(&name, &provider, &key).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn delete_account(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the model account {name}?"),
            "The oracle refuses this while a preset uses the account.".to_owned(),
            "Delete",
            move |this, window, cx| {
                let account = name.clone();
                let task = this.call(cx, move |client| async move {
                    client.delete_model_account(&account).await
                });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn render_login(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.login.as_ref()?;
        let theme = cx.theme().clone();
        let login = &pending.login;
        let mut el = card(cx).debug_selector(|| "settings-login".into()).child(
            div()
                .text_sm()
                .child(format!("Signing in {} ({})", login.account, login.provider)),
        );
        let mut actions = h_flex().gap_2();
        match &login.prompt {
            ModelLoginPrompt::OpenUrl { url, .. } => {
                el = el.child(detail(
                    if pending.catching {
                        "The browser opened the sign-in page. Loom catches the result when you finish. If the browser shows a code, paste it."
                    } else {
                        "The browser opened the sign-in page. When you finish, paste the code or the URL that you get."
                    },
                    cx,
                ));
                let (open, copy) = (url.clone(), url.clone());
                actions = actions
                    .child(
                        small_button("login-open", "Open again")
                            .on_click(move |_, _, cx| cx.open_url(&open)),
                    )
                    .child(
                        small_button("login-copy", "Copy URL").on_click(move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            notify::info(window, cx, "Copied the URL.");
                        }),
                    )
                    .child(
                        small_button("login-paste", "Paste code…")
                            .primary()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.paste_login(window, cx)),
                            ),
                    );
            }
            ModelLoginPrompt::DeviceCode {
                user_code,
                verification_uri,
                expires_in_secs,
            } => {
                el = el
                    .child(detail(
                        format!(
                            "Open {verification_uri} and enter this code. The code expires in {} min.",
                            expires_in_secs / 60
                        ),
                        cx,
                    ))
                    .child(
                        div()
                            .text_xl()
                            .font_family(theme.mono_font_family.clone())
                            .child(user_code.clone()),
                    );
                let (open, code) = (verification_uri.clone(), user_code.clone());
                actions = actions
                    .child(
                        small_button("login-open", "Open page")
                            .on_click(move |_, _, cx| cx.open_url(&open)),
                    )
                    .child(small_button("login-copy", "Copy code").on_click(
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                            notify::info(window, cx, "Copied the code.");
                        },
                    ));
            }
            ModelLoginPrompt::Unknown => {
                el = el.child(danger(
                    "The oracle sent a sign-in step that this version does not know.",
                    cx,
                ));
            }
        }
        actions = actions.child(
            small_button("login-cancel", "Cancel")
                .on_click(cx.listener(|this, _, window, cx| this.cancel_login(window, cx))),
        );
        Some(el.child(actions).into_any_element())
    }

    pub(super) fn render_models(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = now_unix();
        let accounts = self.link.read(cx).config.accounts.clone();
        let add = small_button("add-account", "Add account…")
            .loading(self.is_busy("login"))
            .on_click(cx.listener(|this, _, window, cx| this.add_account(window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Models",
            "The model accounts of the oracle. A preset gives the account of each provider in llm.accounts.",
            vec![add],
            cx,
        ));
        if let Some(login) = self.render_login(cx) {
            page = page.child(login);
        }
        if accounts.is_empty() {
            page = page.child(empty("No model accounts.", cx));
        }
        for (ix, account) in accounts.iter().enumerate() {
            let mut facts = vec![
                account.provider.clone(),
                match account.kind {
                    AccountKind::SignIn => "sign-in".to_owned(),
                    AccountKind::ApiKey => "API key".to_owned(),
                    AccountKind::Unknown => "unknown kind".to_owned(),
                },
            ];
            facts.extend(account.email.clone());
            facts.extend(account.organization.clone());
            facts.push(format!("changed {}", age(now, account.updated_at)));
            facts.push(if account.presets.is_empty() {
                "no presets".to_owned()
            } else {
                format!("presets {}", account.presets.join(", "))
            });
            let sign_in = account.kind == AccountKind::SignIn;
            let device = DEVICE_CODE_PROVIDERS.contains(&account.provider.as_str());
            let (renew, key, delete) = (account.clone(), account.clone(), account.name.clone());
            page = page.child(
                card(cx)
                    .debug_selector(move || format!("settings-account-{ix}"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(account.name.clone()))
                            .when(sign_in, |el| {
                                el.child(
                                    small_button(
                                        SharedString::from(format!("sign-in-{ix}")),
                                        "Sign in again",
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.sign_in(
                                                renew.name.clone(),
                                                renew.provider.clone(),
                                                device,
                                                window,
                                                cx,
                                            )
                                        },
                                    )),
                                )
                            })
                            .when(!sign_in, |el| {
                                el.child(
                                    small_button(
                                        SharedString::from(format!("replace-key-{ix}")),
                                        "Replace key…",
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.replace_key(&key, window, cx)
                                        },
                                    )),
                                )
                            })
                            .child(
                                small_button(
                                    SharedString::from(format!("delete-account-{ix}")),
                                    "Delete…",
                                )
                                .danger()
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.delete_account(delete.clone(), window, cx)
                                    },
                                )),
                            ),
                    )
                    .child(detail(facts.join(" · "), cx))
                    .when_some(account.error.clone(), |el, error| {
                        el.child(danger(format!("{error}. Sign in again."), cx))
                    }),
            );
        }
        page.into_any_element()
    }

    pub(super) fn edit_secret(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = self.link.clone();
        let mut fields = Vec::new();
        if name.is_none() {
            fields.push(Field::text("Name").placeholder("npm-work"));
        }
        fields.push(
            Field::secret("Value")
                .help("You cannot read the value again. The oracle keeps it encrypted."),
        );
        let title = match &name {
            Some(name) => format!("Replace the value of {name}"),
            None => "Add secret".to_owned(),
        };
        form::open(
            title,
            None,
            fields,
            "Save",
            move |values, done, window, cx| {
                let (secret, value) = match &name {
                    Some(name) => (name.clone(), values.raw(0)),
                    None => (values.text(0), values.raw(1)),
                };
                if secret.is_empty() || value.is_empty() {
                    return done.fail("Give the name and the value.", window, cx);
                }
                let task = super::call(&link, cx, move |client| async move {
                    client.set_secret(&secret, &value).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn delete_secret(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the secret {name}?"),
            "The oracle refuses this while a preset uses the secret.".to_owned(),
            "Delete",
            move |this, window, cx| {
                let secret = name.clone();
                let task = this.call(cx, move |client| async move {
                    client.delete_secret(&secret).await
                });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    pub(super) fn render_secrets(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let now = now_unix();
        let link = self.link.read(cx);
        let secrets = link.config.secrets.clone();
        let presets = link.presets.clone();
        let add = small_button("add-secret", "Add secret…")
            .on_click(cx.listener(|this, _, window, cx| this.edit_secret(None, window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Secrets",
            "Values for the environment of codespaces, for example tokens of package registries. A preset gives the secret of each variable in environment.secrets.",
            vec![add],
            cx,
        ));
        if secrets.is_empty() {
            page = page.child(empty("No secrets.", cx));
        }
        for (ix, secret) in secrets.iter().enumerate() {
            let uses = secret_uses(&presets, &secret.name);
            let used = if uses.is_empty() {
                "no presets".to_owned()
            } else {
                uses.iter()
                    .map(|(preset, variable)| format!("{variable} in {preset}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (replace, delete) = (secret.name.clone(), secret.name.clone());
            page = page.child(
                card(cx)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().text_sm().child(secret.name.clone()))
                            .child(
                                small_button(
                                    SharedString::from(format!("replace-secret-{ix}")),
                                    "Replace value…",
                                )
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.edit_secret(Some(replace.clone()), window, cx)
                                    },
                                )),
                            )
                            .child(
                                small_button(
                                    SharedString::from(format!("delete-secret-{ix}")),
                                    "Delete…",
                                )
                                .danger()
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.delete_secret(delete.clone(), window, cx)
                                    },
                                )),
                            ),
                    )
                    .child(detail(
                        format!("changed {} · {used}", age(now, secret.updated_at)),
                        cx,
                    )),
            );
        }
        page.into_any_element()
    }
}
