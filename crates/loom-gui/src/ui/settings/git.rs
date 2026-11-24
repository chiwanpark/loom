use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, SharedString, Styled,
    Window, div, prelude::FluentBuilder,
};
use gpui_component::button::ButtonVariants;
use gpui_component::{Disableable, h_flex, v_flex};
use loom_client::oracle_proto::{
    GitCredential, GitCredentialInput, GitProvider, GitProviderInput, ProviderRoute, Secret,
};
use loom_view_model::settings::{age, credential_presets, expiry_warning, until};

use super::form::{self, Field};
use super::{SettingsView, card, danger, detail, empty, header, now_unix, small_button, warning};
use crate::ui::notify;

impl SettingsView {
    fn edit_provider(
        &mut self,
        provider: Option<&GitProvider>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = self.link.clone();
        let editing = provider.map(|p| p.name.clone());
        let route = provider.map(|p| p.route).unwrap_or_default();
        let routes = if route == ProviderRoute::Hosts {
            vec!["hosts".to_owned(), "direct".to_owned()]
        } else {
            vec!["direct".to_owned(), "hosts".to_owned()]
        };
        let mut fields = Vec::new();
        if editing.is_none() {
            fields.push(Field::text("Name").placeholder("github"));
        }
        fields.extend([
            Field::text("Web URL")
                .placeholder("https://github.com")
                .value(provider.map(|p| p.web_url.clone()).unwrap_or_default()),
            Field::text("API URL")
                .placeholder("https://api.github.com")
                .value(provider.map(|p| p.api_url.clone()).unwrap_or_default())
                .help("Empty for the default: https://api.github.com for github.com, and <web URL>/api/v3 for other servers."),
            Field::text("Git name")
                .placeholder("Your Name")
                .value(provider.and_then(|p| p.git_name.clone()).unwrap_or_default()),
            Field::text("Git email")
                .placeholder("you@example.com")
                .value(provider.and_then(|p| p.git_email.clone()).unwrap_or_default()),
            Field::choice("Route", routes).help(
                "direct: the oracle calls the API. hosts: the oracle sends the calls through a host that reaches the provider.",
            ),
        ]);
        let title = match &editing {
            Some(name) => format!("Git provider {name}"),
            None => "Add Git provider".to_owned(),
        };
        form::open(
            title,
            Some("The clones of each provider use its Git name and email.".to_owned()),
            fields,
            "Save",
            move |values, done, window, cx| {
                let offset = usize::from(editing.is_none());
                let name = editing.clone().unwrap_or_else(|| values.text(0));
                if name.is_empty() {
                    return done.fail("Give a name.", window, cx);
                }
                let input = GitProviderInput {
                    name,
                    web_url: values.text(offset),
                    api_url: values.optional(offset + 1),
                    git_name: values.optional(offset + 2),
                    git_email: values.optional(offset + 3),
                    route: ProviderRoute::parse(&values.text(offset + 4)),
                };
                let task = super::call(&link, cx, move |client| async move {
                    client.set_git_provider(input).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn delete_provider(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the Git provider {name}?"),
            "The oracle refuses this while Git credentials or workspaces use the provider."
                .to_owned(),
            "Delete",
            move |this, window, cx| {
                let provider = name.clone();
                let task = this.call(cx, move |client| async move {
                    client.delete_git_provider(&provider).await
                });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn edit_credential(
        &mut self,
        provider: &str,
        credential: Option<&GitCredential>,
        token_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = self.link.clone();
        let provider = provider.to_owned();
        let editing = credential.cloned();
        let mut fields = Vec::new();
        if editing.is_none() {
            fields.push(Field::text("Name").placeholder("github-work"));
        }
        if !token_only {
            fields.extend([
                Field::text("Owner")
                    .placeholder("acme")
                    .value(credential.and_then(|c| c.owner.clone()).unwrap_or_default())
                    .help("Empty for all owners of the provider. Give an owner, or an owner and a repository, to make the scope smaller."),
                Field::text("Repository")
                    .placeholder("server")
                    .value(credential.and_then(|c| c.repository.clone()).unwrap_or_default()),
            ]);
        }
        fields.push(
            Field::secret("Token")
                .placeholder(if editing.is_some() { "Keep the token" } else { "github_pat_…" })
                .help("A fine-grained personal access token needs Contents (write), Pull requests (write), and Metadata (read). The oracle keeps it encrypted, and you cannot read it again."),
        );
        let title = match (&editing, token_only) {
            (Some(c), true) => format!("Replace the token of {}", c.name),
            (Some(c), false) => format!("Git credential {}", c.name),
            (None, _) => format!("Add a Git credential for {provider}"),
        };
        form::open(
            title,
            None,
            fields,
            "Save",
            move |values, done, window, cx| {
                let mut ix = 0;
                let mut next = || {
                    ix += 1;
                    ix - 1
                };
                let name = match &editing {
                    Some(c) => c.name.clone(),
                    None => values.text(next()),
                };
                if name.is_empty() {
                    return done.fail("Give a name.", window, cx);
                }
                let (owner, repository) = if token_only {
                    let c = editing.as_ref();
                    (
                        c.and_then(|c| c.owner.clone()),
                        c.and_then(|c| c.repository.clone()),
                    )
                } else {
                    (values.optional(next()), values.optional(next()))
                };
                let token = values.optional(next()).map(Secret::new);
                if token.is_none() && (editing.is_none() || token_only) {
                    return done.fail("Give a token.", window, cx);
                }
                let app_id = editing.as_ref().and_then(|c| c.app_id);
                let input = GitCredentialInput {
                    name,
                    provider: provider.clone(),
                    owner,
                    repository,
                    token,
                    app_id,
                };
                let task = super::call(&link, cx, move |client| async move {
                    client.set_git_credential(input).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn check_credential(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let credential = name.clone();
        let task = self.call(cx, move |client| async move {
            client.check_git_credential(&credential).await
        });
        self.act(
            Some(format!("credential:{name}")),
            task,
            |_, credential, window, cx| match &credential.check_error {
                None => notify::success(
                    window,
                    cx,
                    format!(
                        "The token of {} works{}.",
                        credential.name,
                        credential
                            .login
                            .as_ref()
                            .map(|l| format!(" for {l}"))
                            .unwrap_or_default()
                    ),
                ),
                Some(error) => notify::error(
                    window,
                    cx,
                    format!("The token of {} does not work: {error}", credential.name),
                ),
            },
            window,
            cx,
        );
    }

    fn delete_credential(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Delete the Git credential {name}?"),
            "The oracle refuses this while a preset names the credential.".to_owned(),
            "Delete",
            move |this, window, cx| {
                let credential = name.clone();
                let task = this.call(cx, move |client| async move {
                    client.delete_git_credential(&credential).await
                });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn render_credential(
        &self,
        id: &str,
        credential: &GitCredential,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = now_unix();
        let link = self.link.read(cx);
        let presets = credential_presets(&link.presets, &credential.name);
        let mut facts = vec![format!("scope {}", credential.scope())];
        facts.extend(credential.login.as_ref().map(|l| format!("login {l}")));
        match credential.expires_at {
            Some(expires) => facts.push(until(now, expires)),
            None => facts.push("no expiry date".to_owned()),
        }
        if let Some(checked) = credential.checked_at {
            facts.push(format!("checked {}", age(now, checked)));
        }
        facts.push(if presets.is_empty() {
            "no presets".to_owned()
        } else {
            format!("presets {}", presets.join(", "))
        });
        let checking = self.is_busy(&format!("credential:{}", credential.name));
        let name = credential.name.clone();
        let provider = credential.provider.clone();
        let (check, replace, edit, delete) = (
            name.clone(),
            credential.clone(),
            credential.clone(),
            name.clone(),
        );
        let (p1, p2) = (provider.clone(), provider);
        v_flex()
            .pl_4()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().text_sm().child(name))
                    .child(
                        small_button(
                            SharedString::from(format!("check-credential-{id}")),
                            "Check",
                        )
                        .loading(checking)
                        .disabled(checking)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.check_credential(check.clone(), window, cx)
                            },
                        )),
                    )
                    .child(
                        small_button(
                            SharedString::from(format!("replace-token-{id}")),
                            "Replace token…",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.edit_credential(&p1, Some(&replace), true, window, cx)
                            },
                        )),
                    )
                    .child(
                        small_button(
                            SharedString::from(format!("edit-credential-{id}")),
                            "Scope…",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.edit_credential(&p2, Some(&edit), false, window, cx)
                            },
                        )),
                    )
                    .child(
                        small_button(
                            SharedString::from(format!("delete-credential-{id}")),
                            "Delete…",
                        )
                        .danger()
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.delete_credential(delete.clone(), window, cx)
                            },
                        )),
                    ),
            )
            .child(detail(facts.join(" · "), cx))
            .when_some(expiry_warning(now, credential.expires_at), |el, text| {
                el.child(warning(
                    format!("The token {text}. Replace it before it expires."),
                    cx,
                ))
            })
            .when_some(credential.check_error.clone(), |el, error| {
                el.child(danger(format!("The last check failed: {error}"), cx))
            })
            .into_any_element()
    }

    pub(super) fn render_git(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let link = self.link.read(cx);
        let providers = link.config.providers.clone();
        let credentials = link.config.credentials.clone();
        let add = small_button("add-provider", "Add provider…")
            .on_click(cx.listener(|this, _, window, cx| this.edit_provider(None, window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Git providers",
            "The GitHub instances of your repositories, and the Git credentials for each. A preset names the credentials of its codespaces.",
            vec![add],
            cx,
        ));
        if providers.is_empty() {
            page = page.child(empty(
                "No Git providers. Add github.com or your GitHub Enterprise Server.",
                cx,
            ));
        }
        for (ix, provider) in providers.iter().enumerate() {
            let identity = match (&provider.git_name, &provider.git_email) {
                (Some(name), Some(email)) => format!("commits as {name} <{email}>"),
                (Some(name), None) => format!("commits as {name}"),
                (None, Some(email)) => format!("commits as <{email}>"),
                (None, None) => "no Git name and email".to_owned(),
            };
            let (edit, delete, add) = (
                provider.clone(),
                provider.name.clone(),
                provider.name.clone(),
            );
            let mut el = card(cx)
                .debug_selector(move || format!("settings-provider-{ix}"))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().text_sm().child(provider.name.clone()))
                        .child(
                            small_button(
                                SharedString::from(format!("add-credential-{ix}")),
                                "Add credential…",
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.edit_credential(&add, None, false, window, cx)
                                },
                            )),
                        )
                        .child(
                            small_button(
                                SharedString::from(format!("edit-provider-{ix}")),
                                "Edit…",
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.edit_provider(Some(&edit), window, cx)
                                },
                            )),
                        )
                        .child(
                            small_button(
                                SharedString::from(format!("delete-provider-{ix}")),
                                "Delete…",
                            )
                            .danger()
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.delete_provider(delete.clone(), window, cx)
                                },
                            )),
                        ),
                )
                .child(detail(
                    format!(
                        "{} · API {} · route {} · {identity}",
                        provider.web_url, provider.api_url, provider.route
                    ),
                    cx,
                ));
            let mut any = false;
            for (cix, credential) in credentials
                .iter()
                .filter(|c| c.provider == provider.name)
                .enumerate()
            {
                any = true;
                el = el.child(self.render_credential(&format!("{ix}-{cix}"), credential, cx));
            }
            if !any {
                el = el.child(detail("No Git credentials. Without one, codespaces can only read public repositories.", cx));
            }
            page = page.child(el);
        }
        page.into_any_element()
    }
}
