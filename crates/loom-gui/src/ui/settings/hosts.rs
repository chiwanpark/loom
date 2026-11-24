use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, SharedString, Styled,
    Window, div,
};
use gpui_component::button::ButtonVariants;
use gpui_component::{ActiveTheme, Disableable, h_flex, v_flex};
use loom_client::oracle_proto::{DockerSettings, Environment, Host, HostCheckStatus, Role};
use loom_view_model::codespaces::format_memory;
use loom_view_model::settings::{
    age, check_problems, parse_variables, split_certificates, usage_label, variables_text,
    volume_label, volumes_on_host,
};

use super::editor::TextFile;
use super::form::{self, Field};
use super::{SettingsView, card, danger, detail, empty, header, now_unix, small_button, warning};
use crate::ui::notify;

fn report_line(host: &Host) -> Option<String> {
    let report = host.report.as_ref()?;
    let mut parts = vec![
        format!("{} driver", report.driver),
        format!("{} CPUs", report.cpus),
        format_memory(report.memory_mib),
    ];
    parts.extend(
        report
            .storage_driver
            .as_ref()
            .map(|s| format!("storage {s}")),
    );
    parts.extend(report.incus_version.as_ref().map(|v| format!("Incus {v}")));
    parts.extend(report.architecture.clone());
    parts.extend(
        report
            .max_codespaces
            .map(|n| format!("at most {n} codespaces")),
    );
    parts.push(format!("loom-host {}", report.version));
    Some(parts.join(" · "))
}

fn environment_line(environment: &Environment) -> String {
    let mut parts = vec![match &environment.base_image {
        Some(image) => format!("base image {image}"),
        None => "the default base image".to_owned(),
    }];
    let count = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    if !environment.variables.is_empty() {
        parts.push(count(environment.variables.len(), "variable", "variables"));
    }
    if !environment.ca_certificates.is_empty() {
        parts.push(count(
            environment.ca_certificates.len(),
            "CA certificate",
            "CA certificates",
        ));
    }
    if !environment.docker.registry_mirrors.is_empty() {
        parts.push(count(
            environment.docker.registry_mirrors.len(),
            "registry mirror",
            "registry mirrors",
        ));
    }
    parts.push(match &environment.setup_script {
        Some(script) => count(
            script.lines().count(),
            "line of setup script",
            "lines of setup script",
        ),
        None => "no setup script".to_owned(),
    });
    parts.join(" · ")
}

impl SettingsView {
    pub(super) fn load_host_sizes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.read(cx);
        let names: Vec<String> = link
            .hosts
            .iter()
            .filter(|h| h.connected && !volumes_on_host(&link.config.volumes, &h.name).is_empty())
            .map(|h| h.name.clone())
            .collect();
        for name in names {
            let host = name.clone();
            let task = self.call(
                cx,
                move |client| async move { client.host_volumes(&host).await },
            );
            let key = format!("sizes:{name}");
            if self.is_busy(&key) {
                continue;
            }
            self.busy.insert(key.clone());
            cx.spawn_in(
                window,
                async move |this, cx: &mut gpui::AsyncWindowContext| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        this.busy.remove(&key);
                        this.host_sizes.insert(name, result);
                        cx.notify();
                    });
                },
            )
            .detach();
        }
    }

    pub(super) fn check_host(
        &mut self,
        name: String,
        docker: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = name.clone();
        let task = self.call(cx, move |client| async move {
            client.check_host(&host, docker).await
        });
        self.act(
            Some(format!("check:{name}")),
            task,
            move |_, host, window, cx| {
                let problems = check_problems(&host.checks);
                if problems == 0 {
                    notify::success(
                        window,
                        cx,
                        format!("The host {} passed all checks.", host.name),
                    );
                } else {
                    notify::info(
                        window,
                        cx,
                        format!(
                            "The host {} has {problems} check results to look at.",
                            host.name
                        ),
                    );
                }
            },
            window,
            cx,
        );
    }

    fn edit_environment(&mut self, host: &Host, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.clone();
        let name = host.name.clone();
        let current = host.environment.clone();
        form::open(
            format!("Environment of {name}"),
            Some("The host builds a new codespace image after a change. The next codespace waits for the build.".to_owned()),
            vec![
                Field::text("Base image")
                    .placeholder("images:debian/13")
                    .value(current.base_image.clone().unwrap_or_default())
                    .help("Empty for the default image of loom-host."),
                Field::lines("Environment variables", 3)
                    .placeholder("HTTP_PROXY=http://proxy.example.com:3128")
                    .value(variables_text(&current))
                    .help("One NAME=VALUE on each line, for example proxy settings."),
                Field::lines("CA certificates", 3)
                    .placeholder("-----BEGIN CERTIFICATE-----")
                    .value(current.ca_certificates.join("\n")),
                Field::lines("Docker registry mirrors", 2)
                    .placeholder("https://mirror.example.com")
                    .value(current.docker.registry_mirrors.join("\n"))
                    .help("One URL on each line."),
            ],
            "Save",
            move |values, done, window, cx| {
                let variables = match parse_variables(&values.lines(1)) {
                    Ok(variables) => variables,
                    Err(err) => return done.fail(err, window, cx),
                };
                let environment = Environment {
                    base_image: values.optional(0),
                    setup_script: current.setup_script.clone(),
                    variables,
                    ca_certificates: split_certificates(&values.raw(2)),
                    docker: DockerSettings {
                        registry_mirrors: values.lines(3),
                    },
                };
                let host = name.clone();
                let task = super::call(&link, cx, move |client| async move {
                    client.set_host_environment(&host, environment).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn edit_ssh(&mut self, host: &Host, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.link.clone();
        let name = host.name.clone();
        form::open(
            format!("SSH destination of {name}"),
            Some("The desktop client can connect to the codespaces of this host over SSH. Give the machine where loom-host runs. Leave it empty to remove it.".to_owned()),
            vec![Field::text("SSH destination")
                .placeholder("dev@host.example.com")
                .value(host.ssh.clone().unwrap_or_default())],
            "Save",
            move |values, done, window, cx| {
                let destination = values.optional(0);
                let host = name.clone();
                let task = super::call(&link, cx, move |client| async move {
                    client.set_host_ssh(&host, destination.as_deref()).await
                });
                done.wait(task, |_, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn remove_host(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_then(
            format!("Remove the host {name}?"),
            "The host cannot connect to the oracle again. The oracle refuses this while workspaces or codespaces use the host.".to_owned(),
            "Remove",
            move |this, window, cx| {
                let host = name.clone();
                let task = this.call(cx, move |client| async move { client.remove_host(&host).await });
                this.act(None, task, |_, _, _, _| {}, window, cx);
            },
            window,
            cx,
        );
    }

    fn open_script(&mut self, host: &Host, window: &mut Window, cx: &mut Context<Self>) {
        if self.scripts.contains_key(&host.name) {
            return;
        }
        let script = host.environment.setup_script.clone().unwrap_or_default();
        let file = TextFile::new("setup.sh", &script, window, cx);
        self.scripts.insert(host.name.clone(), file);
        cx.notify();
    }

    fn close_script(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.scripts.get(name).is_some_and(|f| f.is_dirty(cx));
        if !dirty {
            self.scripts.remove(name);
            cx.notify();
            return;
        }
        let name = name.to_owned();
        self.confirm_then(
            "Discard your changes?".to_owned(),
            "The changes to the setup script are not saved.".to_owned(),
            "Discard",
            move |this, _, cx| {
                this.scripts.remove(&name);
                cx.notify();
            },
            window,
            cx,
        );
    }

    fn save_script(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.scripts.get(&name) else {
            return;
        };
        let text = file.text(cx);
        let Some(host) = self.link.read(cx).hosts.iter().find(|h| h.name == name) else {
            return;
        };
        let mut environment = host.environment.clone();
        environment.setup_script = Some(text.clone()).filter(|t| !t.trim().is_empty());
        let host = name.clone();
        let task = self.call(cx, move |client| async move {
            client.set_host_environment(&host, environment).await
        });
        self.act(
            Some(format!("script:{name}")),
            task,
            move |this, _, window, cx| {
                if let Some(file) = this.scripts.get_mut(&name) {
                    file.saved = text;
                }
                notify::success(
                    window,
                    cx,
                    "Saved the setup script. The next codespace builds a new image.",
                );
            },
            window,
            cx,
        );
    }

    fn render_host(&self, ix: usize, host: &Host, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let now = now_unix();
        let name = host.name.clone();
        let checking = self.is_busy(&format!("check:{name}"));
        let mut el = card(cx)
            .debug_selector(move || format!("settings-host-{ix}"))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .child(div().text_sm().child(name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if host.connected {
                                        theme.success
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(if host.connected {
                                        "connected"
                                    } else {
                                        "offline"
                                    }),
                            ),
                    )
                    .child({
                        let name = name.clone();
                        small_button(SharedString::from(format!("check-host-{ix}")), "Check")
                            .loading(checking)
                            .disabled(!host.connected || checking)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.check_host(name.clone(), false, window, cx)
                            }))
                    })
                    .child({
                        let name = name.clone();
                        small_button(
                            SharedString::from(format!("check-docker-{ix}")),
                            "Check with Docker",
                        )
                        .disabled(!host.connected || checking)
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.check_host(name.clone(), true, window, cx)
                            },
                        ))
                    })
                    .child({
                        let host = host.clone();
                        small_button(
                            SharedString::from(format!("environment-{ix}")),
                            "Environment…",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.edit_environment(&host, window, cx),
                        ))
                    })
                    .child({
                        let host = host.clone();
                        small_button(
                            SharedString::from(format!("setup-script-{ix}")),
                            "Setup script",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.open_script(&host, window, cx),
                        ))
                    })
                    .child({
                        let host = host.clone();
                        small_button(SharedString::from(format!("ssh-{ix}")), "SSH…").on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.edit_ssh(&host, window, cx)
                            }),
                        )
                    })
                    .child({
                        let name = name.clone();
                        small_button(SharedString::from(format!("remove-host-{ix}")), "Remove…")
                            .danger()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_host(name.clone(), window, cx)
                            }))
                    }),
            );
        let mut facts = Vec::new();
        if let Some(line) = report_line(host) {
            facts.push(line);
        } else {
            facts.push("No report yet. The host sends one when it connects.".to_owned());
        }
        facts.push(format!(
            "{} open codespace{} · added {}",
            host.codespaces,
            if host.codespaces == 1 { "" } else { "s" },
            age(now, host.added_at)
        ));
        facts.push(match &host.ssh {
            Some(ssh) => format!("SSH destination {ssh}"),
            None => "No SSH destination".to_owned(),
        });
        facts.push(environment_line(&host.environment));
        for fact in facts {
            el = el.child(detail(fact, cx));
        }
        for reach in &host.reach {
            el = el.child(match &reach.error {
                None => detail(format!("Reaches the Git provider {}.", reach.name), cx),
                Some(error) => danger(
                    format!("Cannot reach the Git provider {}: {error}", reach.name),
                    cx,
                ),
            });
        }
        if !host.checks.is_empty() {
            let checked = host
                .checked_at
                .map(|t| format!("Checks, {}:", age(now, t)))
                .unwrap_or_else(|| "Checks:".to_owned());
            el = el.child(div().pt_1().text_xs().child(checked));
            for check in &host.checks {
                let text = format!("{} · {}: {}", check.status, check.name, check.detail);
                el = el.child(match check.status {
                    HostCheckStatus::Ok => detail(text, cx),
                    HostCheckStatus::Warning | HostCheckStatus::Unknown => warning(text, cx),
                    HostCheckStatus::Failed => danger(text, cx),
                });
            }
        }
        let link = self.link.read(cx);
        let volumes = volumes_on_host(&link.config.volumes, &host.name);
        if !volumes.is_empty() {
            el = el.child(div().pt_1().text_xs().child("Volumes on this host:"));
            let sizes = self.host_sizes.get(&host.name);
            for volume in volumes {
                let size = match sizes {
                    Some(Ok(usages)) => usages
                        .iter()
                        .find(|u| u.name == volume.name)
                        .map(usage_label),
                    Some(Err(err)) => Some(format!("size unknown: {err}")),
                    None => None,
                };
                let mut text = format!("{} · {}", volume.name, volume_label(volume));
                if let Some(size) = size {
                    text.push_str(&format!(" · {size}"));
                }
                el = el.child(detail(text, cx));
            }
        }
        if let Some(file) = self.scripts.get(&host.name) {
            let dirty = file.is_dirty(cx);
            let saving = self.is_busy(&format!("script:{name}"));
            let (save, close) = (name.clone(), name.clone());
            el = el.child(
                v_flex()
                    .pt_2()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .child(if dirty { "setup.sh (changed)" } else { "setup.sh" }),
                            )
                            .child(
                                small_button(SharedString::from(format!("save-script-{ix}")), "Save")
                                    .primary()
                                    .loading(saving)
                                    .disabled(!dirty || saving)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.save_script(save.clone(), window, cx)
                                    })),
                            )
                            .child(
                                small_button(SharedString::from(format!("close-script-{ix}")), "Close")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.close_script(&close, window, cx)
                                    })),
                            ),
                    )
                    .child(file.render(320.))
                    .child(detail(
                        "The host runs this script as root in a new instance when it builds the codespace image.",
                        cx,
                    )),
            );
        }
        el.into_any_element()
    }

    pub(super) fn render_hosts(&mut self, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let hosts = self.link.read(cx).hosts.clone();
        let add = small_button("add-host", "Add host…")
            .loading(self.is_busy("pair:host"))
            .on_click(
                cx.listener(|this, _, window, cx| this.create_pairing(Role::Host, window, cx)),
            )
            .into_any_element();
        let refresh = small_button("refresh-sizes", "Read volume sizes")
            .on_click(cx.listener(|this, _, window, cx| this.load_host_sizes(window, cx)))
            .into_any_element();
        let mut page = v_flex().gap_2().child(header(
            "Hosts",
            "The machines that run codespaces. Each host runs loom-host. Its local rules stay in the file on the host.",
            vec![refresh, add],
            cx,
        ));
        if let Some(pairing) = self.render_pairing(Role::Host, cx) {
            page = page.child(pairing);
        }
        if hosts.is_empty() {
            page = page.child(empty(
                "No hosts. Click Add host… and run loom-host join with the link on the host.",
                cx,
            ));
        }
        for (ix, host) in hosts.iter().enumerate() {
            page = page.child(self.render_host(ix, host, cx));
        }
        page.into_any_element()
    }
}
