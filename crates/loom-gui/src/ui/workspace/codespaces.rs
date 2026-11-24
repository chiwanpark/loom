use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, AsyncWindowContext, ClickEvent, Context, Entity, FontWeight,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};
use loom_client::NewCodespace;
use loom_client::oracle_proto::{Codespace, CodespaceState, ssh_command};
use loom_client::proto::{PushRepositories, SessionInfo};
use loom_config::HostEntry;
use loom_view_model::codespaces::{
    can_stop, directories, is_starting, new_codespace_defaults, session_info, short_id,
    state_label, title,
};

use super::{AgentConnection, LinkStatus, Workspace, notify};
use crate::connection::{AgentSummary, CodespaceRoute, SshRoute};
use crate::oracle::{OracleEvent, OracleLink, OracleStatus};
use crate::ui::close_codespace;
use crate::ui::new_codespace::{self, NewCodespaceForm, NewCodespaceRequest};
use crate::ui::picker::{PickerItem, open_picker};
use crate::ui::settings::{Page, SettingsView};

type WorkspaceRow = (String, String, Vec<(Codespace, usize)>);

impl Workspace {
    pub(super) fn sync_oracle(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get_mut(ix) else {
            return;
        };
        if host.oracle.is_none() {
            let entry = host.entry.clone();
            let env = self.env.clone();
            let backend = self.backend.clone();
            let link = cx.new(|cx| OracleLink::new(entry, env, backend, cx));
            self.subscriptions
                .push(cx.observe_in(&link, window, |this, link, window, cx| {
                    let name = link.read(cx).name().to_owned();
                    if let Some(ix) = this.hosts.iter().position(|h| h.entry.name() == name) {
                        this.oracle_agents(ix, window, cx);
                    }
                }));
            self.subscriptions.push(cx.subscribe_in(
                &link,
                window,
                |this, link, event: &OracleEvent, window, cx| {
                    this.on_oracle_event(link.clone(), event, window, cx)
                },
            ));
            if let Some(host) = self.hosts.get_mut(ix) {
                host.oracle = Some(link);
            }
        }
        self.oracle_agents(ix, window, cx);
    }

    pub(super) fn refresh_oracle(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(link) = self.hosts.get(ix).and_then(|h| h.oracle.clone()) else {
            return;
        };
        link.update(cx, |link, cx| match link.status {
            OracleStatus::Connected => link.reload(cx),
            _ => link.connect(cx),
        });
        self.oracle_agents(ix, window, cx);
    }

    fn oracle_agents(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(ix) else {
            return;
        };
        let Some(link) = host.oracle.clone() else {
            return;
        };
        let name = host.entry.name().to_owned();
        let link = link.read(cx);
        let mut routes = Vec::new();
        let mut agents = Vec::new();
        for codespace in link.agents() {
            let Some(address) = codespace.agent.clone() else {
                continue;
            };
            let Some(instance) = address.instance.clone() else {
                continue;
            };
            agents.push(AgentSummary {
                instance,
                workspace: Some(format!("{} · {}", codespace.workspace, codespace.branch).into()),
                pid: None,
            });
            let ssh = link
                .hosts
                .iter()
                .find(|host| host.name == codespace.host)
                .and_then(|host| host.ssh.clone())
                .map(|destination| SshRoute {
                    destination,
                    command: ssh_command(&codespace.id),
                });
            routes.push(CodespaceRoute { address, ssh });
        }
        let loading = link.is_connecting();
        let error = match &link.status {
            OracleStatus::Offline(reason) => Some(reason.clone()),
            _ => None,
        };
        self.env.codespaces.set(&name, routes);
        let gone: Vec<String> = self.hosts[ix]
            .agents
            .iter()
            .map(|a| a.instance.clone())
            .filter(|i| !agents.iter().any(|a| &a.instance == i))
            .collect();
        let host = &mut self.hosts[ix];
        host.agents = agents;
        host.loading = loading;
        host.error = error;
        for instance in gone {
            self.connect_errors
                .remove(&(name.clone(), instance.clone()));
            if let Some(conn) = self.find_connection(&name, &instance, cx) {
                let unsaved = !self.unsaved_in(|g| g.connection == conn, cx).is_empty();
                if unsaved {
                    continue;
                }
                self.drop_connection(&conn, window, cx);
            }
        }
        cx.notify();
    }

    fn on_oracle_event(
        &mut self,
        link: Entity<OracleLink>,
        event: &OracleEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let oracle = link.read(cx).name().to_owned();
        match event {
            OracleEvent::Ready(codespace) => notify::success(
                window,
                cx,
                format!("The codespace {} of {oracle} is ready.", title(codespace)),
            ),
            OracleEvent::Failed(codespace) => notify::error(
                window,
                cx,
                format!(
                    "The codespace {} of {oracle} did not start: {}",
                    title(codespace),
                    codespace.error.as_deref().unwrap_or("no reason")
                ),
            ),
            OracleEvent::LoginFinished { .. } => {}
        }
    }

    fn oracle_of(&self, conn: &AgentConnection) -> Option<Entity<OracleLink>> {
        self.hosts
            .iter()
            .find(|h| h.entry.name() == conn.host.name())
            .and_then(|h| h.oracle.clone())
    }

    pub(super) fn codespace_directories(
        &self,
        conn: &Entity<AgentConnection>,
        cx: &App,
    ) -> Vec<String> {
        let read = conn.read(cx);
        let Some(link) = self.oracle_of(read) else {
            return Vec::new();
        };
        link.read(cx)
            .codespace_of_agent(&read.info.instance.0)
            .map(|c| directories(&c.repositories))
            .unwrap_or_default()
    }

    pub(super) fn codespace_sessions(
        &self,
        host: &HostEntry,
        instance: &str,
        cx: &App,
    ) -> Vec<SessionInfo> {
        let Some(link) = self
            .hosts
            .iter()
            .find(|h| h.entry.name() == host.name())
            .and_then(|h| h.oracle.clone())
        else {
            return Vec::new();
        };
        let link = link.read(cx);
        let Some(codespace) = link.codespace_of_agent(instance) else {
            return Vec::new();
        };
        link.sessions
            .iter()
            .filter(|s| s.codespace == codespace.id)
            .map(|s| session_info(&s.session))
            .collect()
    }

    pub(super) fn on_new_codespace(
        &mut self,
        _: &super::NewCodespace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace_picker(window, cx);
    }

    pub(super) fn on_oracle_settings(
        &mut self,
        _: &super::OracleSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let oracles: Vec<usize> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| h.oracle.is_some())
            .map(|(ix, _)| ix)
            .collect();
        match oracles.as_slice() {
            [] => notify::info(
                window,
                cx,
                "Add an oracle first. Click + under Remotes and paste the pairing link of the oracle.",
            ),
            [ix] => self.open_settings(*ix, None, window, cx),
            _ => {
                let this = cx.entity().downgrade();
                let items = oracles
                    .into_iter()
                    .map(|ix| {
                        let this = this.clone();
                        PickerItem::new(
                            self.hosts[ix].entry.name().to_owned(),
                            move |window, cx| {
                                let _ = this
                                    .update(cx, |ws, cx| ws.open_settings(ix, None, window, cx));
                            },
                        )
                    })
                    .collect();
                open_picker("Oracle settings", "Choose an oracle", items, window, cx);
            }
        }
    }

    pub(crate) fn open_settings(
        &mut self,
        hix: usize,
        page: Option<Page>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.hosts.get(hix) else {
            return;
        };
        let Some(link) = host.oracle.clone() else {
            return;
        };
        let name = host.entry.name().to_owned();
        let dirs = self.env.dirs.clone();
        let view = self
            .settings
            .entry(name)
            .or_insert_with(|| cx.new(|cx| SettingsView::new(link, dirs, window, cx)))
            .clone();
        if let Some(page) = page {
            view.update(cx, |view, cx| view.show(page, window, cx));
        }
        gpui::Focusable::focus_handle(view.read(cx), cx).focus(window, cx);
        self.shown_settings = Some(view);
        self.shown_archive = None;
        cx.notify();
    }

    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shown_settings = None;
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    pub(super) fn render_settings(
        &mut self,
        settings: Entity<SettingsView>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let name = settings.read(cx).oracle_name(cx);
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(IconName::Settings).small())
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .child(format!("Settings of the oracle {name}")),
                    )
                    .child(
                        Button::new("close-settings")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close the settings")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_settings(window, cx)),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(settings))
            .into_any_element()
    }

    pub(super) fn workspace_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let links: Vec<Entity<OracleLink>> =
            self.hosts.iter().filter_map(|h| h.oracle.clone()).collect();
        if links.is_empty() {
            notify::info(
                window,
                cx,
                "Add an oracle first. Click + under Remotes and paste the pairing link of the oracle.",
            );
            return;
        }
        let this = cx.entity().downgrade();
        let mut items = Vec::new();
        for link in links {
            let read = link.read(cx);
            for workspace in &read.workspaces {
                let name = workspace.name.clone();
                let link = link.clone();
                let this = this.clone();
                let repositories = directories(&workspace.repositories).join(", ");
                items.push(
                    PickerItem::new(name.clone(), move |window, cx| {
                        let _ = this.update(cx, |ws, cx| {
                            ws.new_codespace_dialog(link.clone(), &name, window, cx)
                        });
                    })
                    .detail(format!("{} · {repositories}", read.name()))
                    .hint(Some(workspace.host.clone())),
                );
            }
        }
        if items.is_empty() {
            notify::info(
                window,
                cx,
                "The oracles have no workspaces. Add one in the Workspaces page of the oracle settings.",
            );
            return;
        }
        open_picker("New codespace", "Choose a workspace", items, window, cx);
    }

    pub(super) fn new_codespace_dialog(
        &mut self,
        link: Entity<OracleLink>,
        workspace: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = link.read(cx);
        let oracle = read.name().to_owned();
        let Some(client) = read.client() else {
            notify::error(
                window,
                cx,
                format!("The client is not connected to the oracle {oracle}."),
            );
            return;
        };
        let read = link.read(cx);
        let Some(found) = read
            .workspaces
            .iter()
            .find(|w| w.name == workspace)
            .cloned()
        else {
            return;
        };
        let form = NewCodespaceForm {
            oracle: read.name().to_owned(),
            host: read.hosts.iter().find(|h| h.name == found.host).cloned(),
            defaults: new_codespace_defaults(&read.codespaces, workspace),
            presets: read.presets.clone(),
            workspace: found,
        };
        let backend = read.backend().clone();
        let this = cx.entity().downgrade();
        new_codespace::open(
            form,
            client,
            backend,
            Rc::new(move |request, window, cx| {
                let _ = this.update(cx, |ws, cx| {
                    ws.create_codespace(link.clone(), request, window, cx)
                });
            }),
            window,
            cx,
        );
    }

    fn create_codespace(
        &mut self,
        link: Entity<OracleLink>,
        request: NewCodespaceRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = link.read(cx);
        let Some(client) = read.client() else {
            notify::error(window, cx, "The client is not connected to the oracle.");
            return;
        };
        let task = read.backend().run(async move {
            client
                .create_codespace_with_warnings(NewCodespace {
                    workspace: request.workspace,
                    preset: request.preset,
                    cpus: Some(request.cpus),
                    memory_mib: Some(request.memory_mib),
                    base_branch: None,
                    base_branches: request.base_branches,
                    client_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                    first_message: request.first_message,
                })
                .await
        });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Some(Ok(created)) => {
                    for warning in &created.warnings {
                        notify::info(window, cx, format!("Warning: {warning}."));
                    }
                    let id = created.codespace.id.clone();
                    link.update(cx, |link, cx| link.upsert_codespace(created.codespace, cx));
                    this.show_progress(link.clone(), id, window, cx);
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
    }

    fn show_progress(
        &mut self,
        link: Entity<OracleLink>,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let open_link = link.clone();
        let open_id = id.clone();
        new_codespace::open_progress(
            link,
            id,
            Rc::new(move |window, cx| {
                let _ = this.update(cx, |ws, cx| {
                    ws.open_codespace(open_link.clone(), &open_id, window, cx)
                });
            }),
            window,
            cx,
        );
    }

    fn with_codespace_agent(
        &mut self,
        link: &Entity<OracleLink>,
        id: &str,
        then: super::AfterConnect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = link.read(cx);
        let name = read.name().to_owned();
        let Some(codespace) = read.codespace(id) else {
            return;
        };
        let state = codespace.state;
        let instance = codespace
            .agent
            .as_ref()
            .and_then(|a| a.instance.clone())
            .filter(|_| state == CodespaceState::Ready);
        let Some(instance) = instance else {
            if state == CodespaceState::Stopped {
                self.resume_codespace(link.clone(), id, window, cx);
                return;
            }
            notify::info(
                window,
                cx,
                format!("The codespace is {state}. Its agent is not ready."),
            );
            return;
        };
        let Some(host_ix) = self.hosts.iter().position(|h| h.entry.name() == name) else {
            return;
        };
        if let Some(conn) = self
            .find_connection(&name, &instance, cx)
            .filter(|c| c.read(cx).status == LinkStatus::Connected)
        {
            self.active_connection = Some(conn.clone());
            then(self, conn, window, cx);
            return;
        }
        self.oracle_agents(host_ix, window, cx);
        self.connect_then(host_ix, instance, then, window, cx);
    }

    fn stop_codespace(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(codespace) = link.read(cx).codespace(id).cloned() else {
            return;
        };
        let stop_link = link.clone();
        let stop_id = codespace.id.clone();
        super::dialogs::confirm(
            "Stop codespace",
            format!(
                "Stop the codespace {}? Its sessions and terminals end. The files stay, and you can resume it later.",
                title(&codespace)
            ),
            "Stop codespace",
            move |window, cx| {
                change_state(stop_link.clone(), stop_id.clone(), Change::Stop, window, cx)
            },
            window,
            cx,
        );
    }

    fn resume_codespace(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(codespace) = link.read(cx).codespace(id).cloned() else {
            return;
        };
        let resume_link = link.clone();
        let resume_id = codespace.id.clone();
        let this = cx.entity().downgrade();
        let deletion = codespace
            .delete_at
            .map(|at| {
                format!(
                    " The oracle deletes it {} if it stays stopped.",
                    loom_view_model::settings::in_time(crate::ui::settings::now_unix(), at)
                )
            })
            .unwrap_or_default();
        super::dialogs::confirm(
            "Resume codespace",
            format!(
                "The codespace {} is stopped. Start it again?{deletion}",
                title(&codespace)
            ),
            "Resume",
            move |window, cx| {
                change_state(
                    resume_link.clone(),
                    resume_id.clone(),
                    Change::Resume,
                    window,
                    cx,
                );
                let (link, id) = (resume_link.clone(), resume_id.clone());
                let _ = this.update(cx, |ws, cx| ws.show_progress(link, id, window, cx));
            },
            window,
            cx,
        );
    }

    fn open_codespace(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_codespace_agent(
            &link,
            id,
            Box::new(|ws, conn, window, cx| {
                let newest = conn
                    .read(cx)
                    .sessions
                    .iter()
                    .filter(|s| !s.archived)
                    .max_by_key(|s| s.created_at)
                    .cloned();
                match newest {
                    Some(info) => ws.open_session(conn, info, window, cx),
                    None => ws.new_session(conn, window, cx),
                }
            }),
            window,
            cx,
        );
    }

    fn codespace_new_session(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_codespace_agent(
            &link,
            id,
            Box::new(|ws, conn, window, cx| ws.new_session(conn, window, cx)),
            window,
            cx,
        );
    }

    fn codespace_pull_request(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let suggested = link
            .read(cx)
            .sessions
            .iter()
            .filter(|s| s.codespace == id)
            .max_by_key(|s| s.session.created_at)
            .and_then(|s| s.session.title.clone())
            .unwrap_or_default();
        self.with_codespace_agent(
            &link,
            id,
            Box::new(move |_, conn, window, cx| {
                let read = conn.read(cx);
                let client = read.client.clone();
                let backend = read.backend.clone();
                let task = backend.run(async move { client.repositories().await });
                let conn = conn.clone();
                window
                    .spawn(cx, async move |cx: &mut AsyncWindowContext| {
                        let result = task.await;
                        let _ = cx.update(|window, cx| match result {
                            Some(Ok(statuses)) => {
                                let conn = conn.clone();
                                close_codespace::open_pull_request(
                                    statuses,
                                    suggested,
                                    Rc::new(move |title, body, draft, window, cx| {
                                        open_pull_requests(
                                            conn.clone(),
                                            title,
                                            body,
                                            draft,
                                            window,
                                            cx,
                                        )
                                    }),
                                    window,
                                    cx,
                                )
                            }
                            Some(Err(err)) => notify::error(window, cx, err.to_string()),
                            None => {}
                        });
                    })
                    .detach();
            }),
            window,
            cx,
        );
    }

    fn close_codespace(
        &mut self,
        link: Entity<OracleLink>,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(codespace) = link.read(cx).codespace(id).cloned() else {
            return;
        };
        if codespace.state != CodespaceState::Ready {
            let close_link = link.clone();
            let close_id = codespace.id.clone();
            super::dialogs::confirm(
                "Close codespace",
                format!(
                    "Close the codespace {}? It is {}, so the client cannot ask its agent for unsaved work. The oracle removes the container and its data.",
                    title(&codespace),
                    codespace.state
                ),
                "Close codespace",
                move |window, cx| close_now(close_link.clone(), close_id.clone(), window, cx),
                window,
                cx,
            );
            return;
        }
        let name = title(&codespace);
        let close_link = link.clone();
        let close_id = codespace.id.clone();
        self.with_codespace_agent(
            &link,
            id,
            Box::new(move |_, conn, window, cx| {
                let read = conn.read(cx);
                let client = read.client.clone();
                let task = read.backend.run(async move { client.repositories().await });
                window
                    .spawn(cx, async move |cx: &mut AsyncWindowContext| {
                        let result = task.await;
                        let _ = cx.update(|window, cx| match result {
                            Some(Ok(statuses)) => {
                                let conn = conn.clone();
                                let link = close_link.clone();
                                let id = close_id.clone();
                                close_codespace::open_close(
                                    name.clone(),
                                    statuses,
                                    Rc::new(move |push, message, window, cx| {
                                        push_then_close(
                                            conn.clone(),
                                            link.clone(),
                                            id.clone(),
                                            push,
                                            message,
                                            window,
                                            cx,
                                        )
                                    }),
                                    window,
                                    cx,
                                )
                            }
                            Some(Err(err)) => notify::error(
                                window,
                                cx,
                                format!("The client cannot read the repositories of the codespace: {err}"),
                            ),
                            None => {}
                        });
                    })
                    .detach();
            }),
            window,
            cx,
        );
    }

    pub(super) fn render_oracles(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut section = v_flex().w_full().gap_0p5().child(
            div()
                .px_1()
                .pt_3()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child("Workspaces"),
        );
        for (hix, host) in self.hosts.iter().enumerate() {
            let Some(link) = host.oracle.clone() else {
                continue;
            };
            let read = link.read(cx);
            let oracle_status = read.status.clone();
            let connecting = read.is_connecting();
            let workspaces: Vec<WorkspaceRow> = read
                .workspaces
                .iter()
                .map(|workspace| {
                    let codespaces = read
                        .listed_codespaces(&workspace.name)
                        .map(|c| (c.clone(), read.session_count(&c.id)))
                        .collect();
                    (
                        workspace.name.clone(),
                        format!(
                            "{} · {}",
                            workspace.host,
                            directories(&workspace.repositories).join(", ")
                        ),
                        codespaces,
                    )
                })
                .collect();
            let color = self.host_color(host.entry.name());
            let status = match &oracle_status {
                OracleStatus::Connecting => "connecting…".to_owned(),
                OracleStatus::Connected => {
                    let count = workspaces.len();
                    format!(
                        "oracle · {count} workspace{}",
                        if count == 1 { "" } else { "s" }
                    )
                }
                OracleStatus::Offline(_) => "oracle · offline".to_owned(),
            };
            section = section.child(
                v_flex()
                    .w_full()
                    .px_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(Icon::new(gpui_kit_assets::IconName::Cloud).xsmall())
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .items_baseline()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_sm()
                                    .child(
                                        div()
                                            .when_some(color, |el, color| el.text_color(color))
                                            .child(host.entry.name().to_owned()),
                                    )
                                    .child(
                                        div()
                                            .ml_1()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(status),
                                    ),
                            )
                            .child(
                                Button::new(("refresh-oracle", hix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::RefreshCw)
                                    .loading(connecting)
                                    .tooltip("Read the lists of the oracle again")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.refresh_oracle(hix, window, cx)
                                    })),
                            )
                            .child(self.render_host_color_menu(hix, cx))
                            .child(
                                Button::new(("oracle-settings", hix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Settings)
                                    .tooltip("Settings of this oracle")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_settings(hix, None, window, cx)
                                    })),
                            )
                            .child(
                                Button::new(("remove-oracle", hix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Delete)
                                    .tooltip("Remove this oracle")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_host(hix, window, cx)
                                    })),
                            ),
                    )
                    .when_some(
                        match &oracle_status {
                            OracleStatus::Offline(reason) => Some(reason.clone()),
                            _ => None,
                        },
                        |el, error| {
                            el.child(
                                div()
                                    .pl_5()
                                    .text_xs()
                                    .text_color(theme.danger)
                                    .whitespace_normal()
                                    .child(error),
                            )
                        },
                    ),
            );
            if oracle_status == OracleStatus::Connected && workspaces.is_empty() {
                section = section.child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child("No workspaces. Add one with loom-oracle-cli workspace add."),
                );
            }
            for (wix, (workspace, repositories, codespaces)) in workspaces.into_iter().enumerate() {
                let new_link = link.clone();
                let name = workspace.clone();
                let workspace_name = workspace.clone();
                section = section.child(
                    h_flex()
                        .id(SharedString::from(format!("oracle-workspace-{hix}-{wix}")))
                        .debug_selector(move || format!("oracle-workspace-{hix}-{wix}"))
                        .w_full()
                        .pl_4()
                        .pr_1()
                        .gap_1()
                        .child(Icon::new(gpui_kit_assets::IconName::FolderGit2).xsmall())
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .items_baseline()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_sm()
                                .child(workspace)
                                .child(
                                    div()
                                        .ml_1()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(repositories),
                                ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("new-codespace-{hix}-{wix}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip("New codespace")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.new_codespace_dialog(new_link.clone(), &name, window, cx)
                                })),
                        ),
                );
                let archives_for = workspace_name.clone();
                for (cix, (codespace, sessions)) in codespaces.into_iter().enumerate() {
                    section = section.child(self.render_codespace(
                        &link,
                        (hix, wix, cix),
                        &codespace,
                        sessions,
                        cx,
                    ));
                }
                if let Some(archives) = self.render_archives(&link, (hix, wix), &archives_for, cx) {
                    section = section.child(archives);
                }
            }
        }
        section.into_any_element()
    }

    fn render_codespace(
        &self,
        link: &Entity<OracleLink>,
        (hix, wix, cix): (usize, usize, usize),
        codespace: &Codespace,
        sessions: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let color = match codespace.state {
            CodespaceState::Ready => theme.success,
            CodespaceState::Failed => theme.danger,
            CodespaceState::Provisioning | CodespaceState::Starting => theme.info,
            CodespaceState::Stopped | CodespaceState::Lost => theme.warning,
            _ => theme.muted_foreground,
        };
        let indicator: AnyElement = if is_starting(codespace.state) || codespace.step.is_some() {
            super::spinner(theme.info)
        } else {
            super::dot(color)
        };
        let detail = format!(
            "{} · {} · {sessions} session{}",
            codespace.preset.as_deref().unwrap_or("no preset"),
            state_label(codespace),
            if sessions == 1 { "" } else { "s" }
        );
        let now = crate::ui::settings::now_unix();
        let mut tooltip = format!(
            "{}\nCodespace {} · {} CPUs · {}\nPreset {}",
            title(codespace),
            short_id(&codespace.id),
            codespace.cpus,
            loom_view_model::codespaces::format_memory(codespace.memory_mib),
            codespace.preset.as_deref().unwrap_or("none")
        );
        if let Some(at) = codespace.stop_at {
            tooltip.push_str(&format!(
                "\nStops when idle {}",
                loom_view_model::settings::in_time(now, at)
            ));
        }
        if let Some(at) = codespace.delete_at {
            tooltip.push_str(&format!(
                "\nThe oracle deletes it {}",
                loom_view_model::settings::in_time(now, at)
            ));
        }
        let tooltip = SharedString::from(tooltip);
        let id = codespace.id.clone();
        let click_link = link.clone();
        let ready = codespace.state == CodespaceState::Ready;
        let stopped = codespace.state == CodespaceState::Stopped;
        let stoppable = can_stop(codespace);
        let starting = is_starting(codespace.state) || codespace.state == CodespaceState::Failed;
        h_flex()
            .id(SharedString::from(format!("codespace-{hix}-{wix}-{cix}")))
            .w_full()
            .pl_6()
            .pr_1()
            .py_0p5()
            .gap_2()
            .rounded_md()
            .hover(|s| s.bg(theme.list_hover))
            .cursor_pointer()
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if ready {
                    this.open_codespace(click_link.clone(), &id, window, cx)
                } else if stopped {
                    this.resume_codespace(click_link.clone(), &id, window, cx)
                } else {
                    this.show_progress(click_link.clone(), id.clone(), window, cx)
                }
            }))
            .child(indicator)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_sm()
                            .child(codespace.branch.clone()),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(if codespace.state == CodespaceState::Failed {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(detail),
                    ),
            )
            .child(self.render_codespace_menu(
                link.clone(),
                codespace.id.clone(),
                (hix, wix, cix),
                MenuState {
                    ready,
                    starting,
                    stopped,
                    stoppable,
                },
                cx,
            ))
            .into_any_element()
    }

    fn render_codespace_menu(
        &self,
        link: Entity<OracleLink>,
        id: String,
        (hix, wix, cix): (usize, usize, usize),
        state: MenuState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let MenuState {
            ready,
            starting,
            stopped,
            stoppable,
        } = state;
        let this = cx.entity().downgrade();
        Button::new(SharedString::from(format!(
            "codespace-menu-{hix}-{wix}-{cix}"
        )))
        .ghost()
        .xsmall()
        .icon(IconName::Ellipsis)
        .tooltip("Codespace actions")
        .dropdown_menu(move |menu, _, _| {
            let action = |run: fn(
                &mut Workspace,
                Entity<OracleLink>,
                &str,
                &mut Window,
                &mut Context<Workspace>,
            )| {
                let this = this.clone();
                let link = link.clone();
                let id = id.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |ws, cx| run(ws, link.clone(), &id, window, cx));
                }
            };
            menu.item(
                PopupMenuItem::new("New session")
                    .disabled(!ready)
                    .on_click(action(Workspace::codespace_new_session)),
            )
            .item(
                PopupMenuItem::new("Open pull request…")
                    .disabled(!ready)
                    .on_click(action(Workspace::codespace_pull_request)),
            )
            .item(
                PopupMenuItem::new("Show start progress")
                    .disabled(!starting)
                    .on_click(action(|ws, link, id, window, cx| {
                        ws.show_progress(link, id.to_owned(), window, cx)
                    })),
            )
            .separator()
            .item(
                PopupMenuItem::new("Stop codespace…")
                    .disabled(!stoppable)
                    .on_click(action(Workspace::stop_codespace)),
            )
            .item(
                PopupMenuItem::new("Resume codespace…")
                    .disabled(!stopped)
                    .on_click(action(Workspace::resume_codespace)),
            )
            .item(
                PopupMenuItem::new("Close codespace…").on_click(action(Workspace::close_codespace)),
            )
        })
        .into_any_element()
    }
}

#[derive(Clone, Copy)]
struct MenuState {
    ready: bool,
    starting: bool,
    stopped: bool,
    stoppable: bool,
}

#[derive(Clone, Copy)]
enum Change {
    Stop,
    Resume,
}

fn change_state(
    link: Entity<OracleLink>,
    id: String,
    change: Change,
    window: &mut Window,
    cx: &mut App,
) {
    let read = link.read(cx);
    let Some(client) = read.client() else {
        notify::error(window, cx, "The client is not connected to the oracle.");
        return;
    };
    let task = read.backend().run(async move {
        match change {
            Change::Stop => client.stop_codespace(&id).await,
            Change::Resume => client.resume_codespace(&id).await,
        }
    });
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Some(Ok(codespace)) => {
                    link.update(cx, |link, cx| {
                        if matches!(change, Change::Resume) {
                            link.progress.remove(&codespace.id);
                        }
                        link.upsert_codespace(codespace, cx)
                    });
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
}

fn open_pull_requests(
    conn: Entity<AgentConnection>,
    title: String,
    body: String,
    draft: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let read = conn.read(cx);
    let client = read.client.clone();
    let task = read
        .backend
        .run(async move { client.open_pull_requests(&title, &body, draft).await });
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Some(Ok(results)) => {
                    let (lines, urls) = close_codespace::pull_request_summary(&results);
                    let failed = results.iter().any(|r| r.error.is_some());
                    let text = lines.join("\n");
                    if failed {
                        notify::error(window, cx, text);
                    } else {
                        notify::success(window, cx, text);
                    }
                    if let Some(url) = urls.first() {
                        cx.open_url(url);
                    }
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
}

fn push_then_close(
    conn: Entity<AgentConnection>,
    link: Entity<OracleLink>,
    id: String,
    push: Vec<String>,
    message: Option<String>,
    window: &mut Window,
    cx: &mut App,
) {
    if push.is_empty() {
        close_now(link, id, window, cx);
        return;
    }
    let read = conn.read(cx);
    let client = read.client.clone();
    let task = read.backend.run(async move {
        client
            .push_repositories(PushRepositories {
                commit_message: message,
                directories: push,
            })
            .await
    });
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Some(Ok(statuses)) => {
                    let failed: Vec<String> = statuses
                        .iter()
                        .filter(|s| s.error.is_some() || s.has_unsaved_work())
                        .map(|s| {
                            format!(
                                "{}: {}",
                                s.directory,
                                loom_view_model::codespaces::work_text(s)
                            )
                        })
                        .collect();
                    if failed.is_empty() {
                        close_now(link.clone(), id.clone(), window, cx);
                    } else {
                        notify::error(
                            window,
                            cx,
                            format!(
                                "The push failed, so the codespace stays open.\n{}",
                                failed.join("\n")
                            ),
                        );
                    }
                }
                Some(Err(err)) => notify::error(
                    window,
                    cx,
                    format!("The push failed, so the codespace stays open: {err}"),
                ),
                None => {}
            });
        })
        .detach();
}

fn close_now(link: Entity<OracleLink>, id: String, window: &mut Window, cx: &mut App) {
    let read = link.read(cx);
    let Some(client) = read.client() else {
        notify::error(window, cx, "The client is not connected to the oracle.");
        return;
    };
    let task = read
        .backend()
        .run(async move { client.close_codespace(&id).await });
    window
        .spawn(cx, async move |cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Some(Ok(codespace)) => {
                    let text = format!(
                        "The codespace {} is {}.",
                        title(&codespace),
                        codespace.state
                    );
                    link.update(cx, |link, cx| link.upsert_codespace(codespace, cx));
                    notify::info(window, cx, text);
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
}
