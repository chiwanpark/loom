use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext, AsyncWindowContext, Context, Entity, FocusHandle, Focusable,
    FontWeight, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, actions, div, prelude::FluentBuilder,
    px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable, TitleBar, h_flex, h_resizable, resizable_panel, v_flex,
    v_resizable,
};
use loom_client::proto::{SessionInfo, SessionState};
use loom_config::{ClientConfig, HostEntry};

use crate::backend::Backend;
use crate::connection::{self, AgentSummary, Env, Target};
use crate::conversation::short_id;
use crate::state::{AgentConnection, ConnectionEvent, LinkStatus, RemoteTerminal};
use crate::term::{TerminalIo, TerminalView, mono_font};
use crate::ui::dialogs::{self, FormField, open_form};
use crate::ui::notify;
use crate::ui::session::SessionView;

actions!(
    loom,
    [
        ToggleLeftDock,
        ToggleBottomDock,
        NewSession,
        NewTerminal,
        CloseTab,
        NextTab,
        PreviousTab,
        ShowProviders,
        ChooseModel,
        AddSshHost,
        PairRelay,
        Quit
    ]
);

const CONTEXT: &str = "Workspace";
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const RECONNECT_ATTEMPTS: usize = 20;
const OUTSIDE_TERMINAL: &str = "Workspace && !Terminal";

pub fn bind_keys(overrides: &BTreeMap<String, String>, cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-b", ToggleLeftDock, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-j", ToggleBottomDock, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-n", NewSession, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-w", CloseTab, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-`", NewTerminal, Some(CONTEXT)),
        KeyBinding::new("ctrl-pagedown", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-pageup", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-m", ChooseModel, Some(CONTEXT)),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
    let mut custom = Vec::new();
    for (keys, name) in overrides {
        match cx.build_action(name, None) {
            Ok(action) => match KeyBinding::load(keys, action, None, false, None, cx) {
                Ok(binding) => custom.push(binding),
                Err(err) => tracing::warn!("invalid keystroke {keys:?} in client.yaml: {err}"),
            },
            Err(err) => tracing::warn!("unknown action {name:?} in client.yaml: {err}"),
        }
    }
    cx.bind_keys(custom);
}

struct HostNode {
    entry: HostEntry,
    agents: Vec<AgentSummary>,
    loading: bool,
    error: Option<String>,
    expanded: bool,
}

struct TerminalTab {
    view: Entity<TerminalView>,
    connection: Entity<AgentConnection>,
}

pub struct Workspace {
    backend: Backend,
    env: Env,
    config: ClientConfig,
    config_path: PathBuf,
    hosts: Vec<HostNode>,
    connections: Vec<Entity<AgentConnection>>,
    connecting: HashSet<(String, String)>,
    active_connection: Option<Entity<AgentConnection>>,
    tabs: Vec<Entity<SessionView>>,
    active_tab: usize,
    terminals: Vec<TerminalTab>,
    active_terminal: usize,
    show_left: bool,
    show_bottom: bool,
    focus: FocusHandle,
    subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        backend: Backend,
        env: Env,
        config: ClientConfig,
        config_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hosts = config
            .hosts
            .iter()
            .map(|entry| HostNode {
                entry: entry.clone(),
                agents: Vec::new(),
                loading: false,
                error: None,
                expanded: true,
            })
            .collect();
        let mut this = Self {
            backend,
            env,
            config,
            config_path,
            hosts,
            connections: Vec::new(),
            connecting: HashSet::new(),
            active_connection: None,
            tabs: Vec::new(),
            active_tab: 0,
            terminals: Vec::new(),
            active_terminal: 0,
            show_left: true,
            show_bottom: false,
            focus: cx.focus_handle(),
            subscriptions: Vec::new(),
        };
        for ix in 0..this.hosts.len() {
            this.refresh_host(ix, window, cx);
        }
        this.focus.focus(window, cx);
        this
    }

    fn save_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.config.hosts = self.hosts.iter().map(|h| h.entry.clone()).collect();
        if let Err(err) = self.config.save(&self.config_path) {
            notify::error(
                window,
                cx,
                format!("Could not save {}: {err}", self.config_path.display()),
            );
        }
    }

    fn refresh_host(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get_mut(ix) else {
            return;
        };
        host.loading = true;
        host.error = None;
        let entry = host.entry.clone();
        let env = self.env.clone();
        let task = self
            .backend
            .run(async move { connection::list_agents(&env, &entry).await });
        let name = host.entry.name().to_owned();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(host) = this.hosts.iter_mut().find(|h| h.entry.name() == name) {
                    host.loading = false;
                    match result {
                        Some(Ok(agents)) => host.agents = agents,
                        Some(Err(err)) => host.error = Some(err),
                        None => {}
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn find_connection(
        &self,
        host: &str,
        instance: &str,
        cx: &App,
    ) -> Option<Entity<AgentConnection>> {
        self.connections
            .iter()
            .find(|c| {
                let c = c.read(cx);
                c.host.name() == host && c.info.instance.0 == instance
            })
            .cloned()
    }

    fn connect(
        &mut self,
        host_ix: usize,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect_with(host_ix, target, true, window, cx);
    }

    fn connect_with(
        &mut self,
        host_ix: usize,
        target: Target,
        report_errors: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.hosts.get(host_ix) else {
            return;
        };
        let entry = host.entry.clone();
        let key = (
            entry.name().to_owned(),
            match &target {
                Target::Instance(id) => id.clone(),
                Target::Workspace(path) | Target::Attach(path) => path.display().to_string(),
            },
        );
        if !self.connecting.insert(key.clone()) {
            return;
        }
        let env = self.env.clone();
        let backend = self.backend.clone();
        let task = {
            let entry = entry.clone();
            self.backend
                .run(async move { connection::connect(&env, &entry, &target).await })
        };
        cx.notify();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.connecting.remove(&key);
                match result {
                    Some(Ok((client, notifications))) => {
                        let instance = client.agent().instance.0.clone();
                        let workspace = client.agent().workspace.clone();
                        let existing =
                            this.find_connection(entry.name(), &instance, cx)
                                .or_else(|| {
                                    this.connections
                                        .iter()
                                        .find(|c| {
                                            let c = c.read(cx);
                                            c.host.name() == entry.name()
                                                && c.info.workspace == workspace
                                                && matches!(c.status, LinkStatus::Disconnected(_))
                                        })
                                        .cloned()
                                });
                        let conn = match existing {
                            Some(conn) => {
                                conn.update(cx, |conn, cx| {
                                    conn.reattach(client, notifications, cx)
                                });
                                conn
                            }
                            None => {
                                let conn = cx.new(|cx| {
                                    AgentConnection::new(
                                        entry.clone(),
                                        client,
                                        notifications,
                                        backend.clone(),
                                        cx,
                                    )
                                });
                                this.watch_connection(&conn, window, cx);
                                this.connections.push(conn.clone());
                                conn
                            }
                        };
                        this.active_connection = Some(conn);
                        if let Some(ix) = this.hosts.iter().position(|h| h.entry == entry) {
                            this.refresh_host(ix, window, cx);
                        }
                    }
                    Some(Err(err)) if report_errors => notify::error(window, cx, err),
                    Some(Err(err)) => tracing::info!("reconnect failed: {err}"),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn schedule_reconnect(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let mut delay = RECONNECT_INITIAL;
            for _ in 0..RECONNECT_ATTEMPTS {
                cx.background_executor().timer(delay).await;
                let keep_going = this
                    .update_in(cx, |this, window, cx| {
                        if !this.connections.contains(&conn)
                            || conn.read(cx).status == LinkStatus::Connected
                        {
                            return false;
                        }
                        let target = reconnect_target(conn.read(cx));
                        let host = conn.read(cx).host.name().to_owned();
                        if let Some(ix) = this.hosts.iter().position(|h| h.entry.name() == host) {
                            this.connect_with(ix, target, false, window, cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
                delay = (delay * 2).min(RECONNECT_MAX);
            }
        })
        .detach();
    }

    fn watch_connection(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscriptions
            .push(cx.observe(conn, |_, _, cx| cx.notify()));
        self.subscriptions.push(cx.subscribe_in(
            conn,
            window,
            |this, conn, event: &ConnectionEvent, window, cx| match event {
                ConnectionEvent::Notice(notice) => {
                    notify::info(window, cx, format!("{notice:?}"));
                }
                ConnectionEvent::Closed => {
                    if this.connections.contains(conn) {
                        let title = conn.read(cx).title();
                        notify::error(
                            window,
                            cx,
                            format!("Disconnected from {title}; reconnecting…"),
                        );
                        this.schedule_reconnect(conn.clone(), window, cx);
                    }
                }
                ConnectionEvent::LoginFinished(_) => {}
            },
        ));
    }

    fn reconnect(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = reconnect_target(conn.read(cx));
        let host = conn.read(cx).host.name().to_owned();
        if let Some(ix) = self.hosts.iter().position(|h| h.entry.name() == host) {
            self.connect(ix, target, window, cx);
        }
    }

    fn open_session(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_connection = Some(conn.clone());
        if let Some(ix) = self.tabs.iter().position(|tab| {
            let tab = tab.read(cx);
            tab.connection == conn && tab.session_id(cx) == info.id
        }) {
            self.activate_tab(ix, window, cx);
            return;
        }
        let conversation = conn.update(cx, |conn, cx| conn.conversation(&info, cx));
        let view = cx.new(|cx| SessionView::new(conn.clone(), conversation, window, cx));
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        self.tabs.push(view);
        self.activate_tab(self.tabs.len() - 1, window, cx);
    }

    fn activate_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active_tab = ix;
        let tab = self.tabs[ix].clone();
        self.active_connection = Some(tab.read(cx).connection.clone());
        let prompt = tab.read(cx).prompt();
        prompt.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ix);
        let (conn, id) = {
            let view = tab.read(cx);
            (view.connection.clone(), view.session_id(cx))
        };
        let still_open = self.tabs.iter().any(|t| {
            let t = t.read(cx);
            t.connection == conn && t.session_id(cx) == id
        });
        if !still_open {
            conn.update(cx, |conn, _| conn.release_conversation(&id));
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        if !self.tabs.is_empty() {
            self.activate_tab(self.active_tab, window, cx);
        }
        cx.notify();
    }

    fn new_session(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = conn.read(cx);
        let client = read.client.clone();
        let task = read
            .backend
            .run(async move { client.create_session(None).await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Some(Ok(info)) => {
                    conn.update(cx, |conn, cx| conn.update_session_info(info.clone(), cx));
                    this.open_session(conn.clone(), info, window, cx);
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
    }

    fn new_terminal(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = conn.read(cx);
        let client = read.client.clone();
        let backend = read.backend.clone();
        let task = read
            .backend
            .run(async move { client.open_terminal(80, 24).await });
        let appearance = self.config.appearance.clone();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Some(Ok(id)) => {
                    let client = conn.read(cx).client.clone();
                    let io: Rc<dyn TerminalIo> = Rc::new(RemoteTerminal::new(&backend, client, id));
                    let font = mono_font(appearance.mono_font.as_deref(), cx);
                    let size = px(appearance.mono_font_size);
                    let view = cx.new(|cx| TerminalView::new(Some(io), font, size, cx));
                    conn.update(cx, |conn, _| conn.register_terminal(id, &view));
                    this.subscriptions
                        .push(cx.observe(&view, |_, _, cx| cx.notify()));
                    this.terminals.push(TerminalTab {
                        view: view.clone(),
                        connection: conn.clone(),
                    });
                    this.active_terminal = this.terminals.len() - 1;
                    this.show_bottom = true;
                    view.read(cx).focus_handle(cx).focus(window, cx);
                    cx.notify();
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
    }

    fn close_terminal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.terminals.len() {
            self.terminals.remove(ix);
            if self.active_terminal >= self.terminals.len() {
                self.active_terminal = self.terminals.len().saturating_sub(1);
            }
            cx.notify();
        }
    }

    fn stop_agent(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = conn.read(cx);
        let client = read.client.clone();
        let instance = read.info.instance.clone();
        let host = read.host.name().to_owned();
        let task = read
            .backend
            .run(async move { client.stop_agent(&instance).await });
        self.tabs.retain(|t| t.read(cx).connection != conn);
        self.terminals.retain(|t| t.connection != conn);
        self.connections.retain(|c| c != &conn);
        if self.active_connection.as_ref() == Some(&conn) {
            self.active_connection = self.connections.first().cloned();
        }
        self.active_tab = 0;
        self.active_terminal = 0;
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(Err(err)) = result
                    && !matches!(err, loom_client::ClientError::Closed)
                {
                    notify::error(window, cx, err.to_string());
                }
                if let Some(ix) = this.hosts.iter().position(|h| h.entry.name() == host) {
                    this.refresh_host(ix, window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn spawn_dialog(&mut self, host_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        let name = self.hosts[host_ix].entry.name().to_owned();
        open_form(
            "Start an agent",
            Some(format!(
                "Start an agent on {name} for a workspace directory. The path is on the agent host."
            )),
            vec![FormField::new("Workspace directory", "/home/me/project")],
            "Start",
            move |values, window, cx| {
                let path = values.into_iter().next().unwrap_or_default();
                if path.is_empty() {
                    return;
                }
                this.update(cx, |ws, cx| {
                    ws.connect(host_ix, Target::Workspace(PathBuf::from(path)), window, cx)
                });
            },
            window,
            cx,
        );
    }

    fn add_ssh_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        open_form(
            "Add an SSH host",
            Some("Loom runs `loom-agent stdio` on the host over your SSH configuration.".into()),
            vec![
                FormField::new("Name", "devbox"),
                FormField::new("SSH destination", "user@devbox"),
                FormField::new("Remote loom-agent path (optional)", "loom-agent"),
            ],
            "Add",
            move |values, window, cx| {
                let [name, destination, remote_bin] =
                    <[String; 3]>::try_from(values).unwrap_or_default();
                this.update(cx, |ws, cx| {
                    let entry = HostEntry::Ssh {
                        name,
                        destination,
                        remote_bin: (!remote_bin.is_empty()).then_some(remote_bin),
                    };
                    ws.add_host(entry, window, cx);
                });
            },
            window,
            cx,
        );
    }

    fn pair_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        open_form(
            "Pair through a relay",
            Some("Run `loom-agent pair` on the agent host and paste the pairing link here.".into()),
            vec![
                FormField::new("Name", "home"),
                FormField::new("Pairing link", "loom://relay.example.com/…"),
                FormField::new("Client label", "").value(connection::default_label()),
            ],
            "Pair",
            move |values, window, cx| {
                let [name, uri, label] = <[String; 3]>::try_from(values).unwrap_or_default();
                this.update(cx, |ws, cx| ws.pair(name, uri, label, window, cx));
            },
            window,
            cx,
        );
    }

    fn pair(
        &mut self,
        name: String,
        uri: String,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let env = self.env.clone();
        let task = self
            .backend
            .run(async move { connection::pair(&env, &uri, &label, &name).await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Some(Ok(entry)) => {
                    notify::success(window, cx, format!("Paired with {}", entry.name()));
                    this.add_host(entry, window, cx);
                }
                Some(Err(err)) => notify::error(window, cx, format!("Pairing failed: {err}")),
                None => {}
            });
        })
        .detach();
    }

    fn add_host(&mut self, entry: HostEntry, window: &mut Window, cx: &mut Context<Self>) {
        let mut config = self.config.clone();
        config.hosts.push(entry.clone());
        if let Err(err) = config.validate("client.yaml") {
            notify::error(window, cx, err.to_string());
            return;
        }
        self.hosts.push(HostNode {
            entry,
            agents: Vec::new(),
            loading: false,
            error: None,
            expanded: true,
        });
        self.save_config(window, cx);
        self.refresh_host(self.hosts.len() - 1, window, cx);
    }

    fn remove_host(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.hosts.len() {
            self.hosts.remove(ix);
            self.save_config(window, cx);
            cx.notify();
        }
    }

    fn active_session(&self) -> Option<&Entity<SessionView>> {
        self.tabs.get(self.active_tab)
    }

    fn on_toggle_left(&mut self, _: &ToggleLeftDock, _: &mut Window, cx: &mut Context<Self>) {
        self.show_left = !self.show_left;
        cx.notify();
    }

    fn on_toggle_bottom(&mut self, _: &ToggleBottomDock, _: &mut Window, cx: &mut Context<Self>) {
        self.show_bottom = !self.show_bottom;
        cx.notify();
    }

    fn on_new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_connection.clone() {
            Some(conn) => self.new_session(conn, window, cx),
            None => notify::info(window, cx, "Connect to an agent first."),
        }
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_connection.clone() {
            Some(conn) => self.new_terminal(conn, window, cx),
            None => notify::info(window, cx, "Connect to an agent first."),
        }
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(self.active_tab, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.activate_tab((self.active_tab + 1) % self.tabs.len(), window, cx);
        }
    }

    fn on_previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            let len = self.tabs.len();
            self.activate_tab((self.active_tab + len - 1) % len, window, cx);
        }
    }

    fn on_providers(&mut self, _: &ShowProviders, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(conn) = self.active_connection.clone() {
            dialogs::open_providers(conn, window, cx);
        }
    }

    fn on_choose_model(&mut self, _: &ChooseModel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_session().cloned() {
            let (conn, id) = {
                let tab = tab.read(cx);
                (tab.connection.clone(), tab.session_id(cx))
            };
            dialogs::open_model_picker(conn, Some(id), window, cx);
        }
    }

    fn on_add_ssh(&mut self, _: &AddSshHost, window: &mut Window, cx: &mut Context<Self>) {
        self.add_ssh_dialog(window, cx);
    }

    fn on_pair(&mut self, _: &PairRelay, window: &mut Window, cx: &mut Context<Self>) {
        self.pair_dialog(window, cx);
    }

    fn render_agents_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut list = v_flex().w_full().gap_1().p_2();
        for (hix, host) in self.hosts.iter().enumerate() {
            let kind = match &host.entry {
                HostEntry::Local { .. } => "local".to_owned(),
                HostEntry::Ssh { destination, .. } => format!("ssh {destination}"),
                HostEntry::Relay { .. } => "relay".to_owned(),
            };
            let expanded = host.expanded;
            list = list.child(
                h_flex()
                    .id(("host", hix))
                    .w_full()
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .hover(|s| s.bg(theme.list_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(host) = this.hosts.get_mut(hix) {
                            host.expanded = !host.expanded;
                        }
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .xsmall(),
                    )
                    .child(Icon::new(IconName::HardDrive).small())
                    .child(
                        v_flex()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(host.entry.name().to_owned()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(kind),
                            ),
                    )
                    .child(
                        Button::new(("refresh", hix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .loading(host.loading)
                            .tooltip("Refresh agents")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.refresh_host(hix, window, cx)
                            })),
                    )
                    .child(
                        Button::new(("spawn", hix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("Start an agent for a workspace")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.spawn_dialog(hix, window, cx)
                            })),
                    )
                    .when(!matches!(host.entry, HostEntry::Local { .. }), |el| {
                        el.child(
                            Button::new(("remove-host", hix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Delete)
                                .tooltip("Remove this host")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.remove_host(hix, window, cx)
                                })),
                        )
                    }),
            );
            if !expanded {
                continue;
            }
            if let Some(error) = &host.error {
                list = list.child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(theme.danger)
                        .whitespace_normal()
                        .child(error.clone()),
                );
            }
            if host.agents.is_empty() && !host.loading && host.error.is_none() {
                list = list.child(
                    div()
                        .pl_6()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("No running agents. Use + to start one."),
                );
            }
            for (aix, agent) in host.agents.iter().enumerate() {
                let conn = self.find_connection(host.entry.name(), &agent.instance, cx);
                let connecting = self
                    .connecting
                    .contains(&(host.entry.name().to_owned(), agent.instance.clone()));
                let label = agent
                    .workspace
                    .as_ref()
                    .map(|w| w.display().to_string())
                    .unwrap_or_else(|| agent.instance.clone());
                let instance = agent.instance.clone();
                let status_color = match conn.as_ref().map(|c| c.read(cx).status.clone()) {
                    Some(LinkStatus::Connected) => theme.success,
                    Some(LinkStatus::Disconnected(_)) => theme.danger,
                    None => theme.muted_foreground,
                };
                let row_conn = conn.clone();
                let mut row = h_flex()
                    .id(SharedString::from(format!("agent-{hix}-{aix}")))
                    .w_full()
                    .gap_1()
                    .pl_5()
                    .pr_1()
                    .rounded_sm()
                    .hover(|s| s.bg(theme.list_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| match &row_conn {
                        Some(conn) => {
                            if matches!(conn.read(cx).status, LinkStatus::Disconnected(_)) {
                                this.reconnect(conn, window, cx);
                            } else {
                                this.active_connection = Some(conn.clone());
                                cx.notify();
                            }
                        }
                        None => this.connect(hix, Target::Instance(instance.clone()), window, cx),
                    }))
                    .child(div().size(px(8.)).rounded_full().bg(status_color))
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_sm()
                            .child(label),
                    );
                if connecting {
                    row = row.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("connecting…"),
                    );
                }
                if let Some(conn) = conn.clone() {
                    let (c1, c2, c3, c4) = (conn.clone(), conn.clone(), conn.clone(), conn.clone());
                    row = row
                        .child(
                            Button::new(SharedString::from(format!("new-session-{hix}-{aix}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip("New session")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.new_session(c1.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("terminal-{hix}-{aix}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::SquareTerminal)
                                .tooltip("New terminal")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.new_terminal(c2.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("providers-{hix}-{aix}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::CircleUser)
                                .tooltip("Providers and sign-in")
                                .on_click(cx.listener(move |_, _, window, cx| {
                                    cx.stop_propagation();
                                    dialogs::open_providers(c3.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("stop-{hix}-{aix}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Square)
                                .tooltip("Stop this agent")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.stop_agent(c4.clone(), window, cx)
                                })),
                        );
                }
                list = list.child(row);
                if let Some(conn) = conn {
                    let sessions = conn.read(cx).sessions.clone();
                    for (six, session) in sessions.into_iter().enumerate() {
                        let open_conn = conn.clone();
                        let title = session
                            .title
                            .clone()
                            .unwrap_or_else(|| format!("Session {}", short_id(&session.id.0)));
                        let busy = session.state != SessionState::Idle;
                        let info = session.clone();
                        let active = self.active_session().is_some_and(|tab| {
                            let tab = tab.read(cx);
                            tab.connection == conn && tab.session_id(cx) == session.id
                        });
                        list = list.child(
                            h_flex()
                                .id(SharedString::from(format!("session-{hix}-{aix}-{six}")))
                                .w_full()
                                .gap_1()
                                .pl_8()
                                .pr_1()
                                .rounded_sm()
                                .when(active, |el| el.bg(theme.list_active))
                                .hover(|s| s.bg(theme.list_hover))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_session(open_conn.clone(), info.clone(), window, cx)
                                }))
                                .child(
                                    div()
                                        .flex_1()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_sm()
                                        .child(title),
                                )
                                .when(busy, |el| {
                                    el.child(
                                        Icon::new(IconName::LoaderCircle)
                                            .xsmall()
                                            .text_color(theme.info),
                                    )
                                }),
                        );
                    }
                }
            }
        }
        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Agents"),
                    )
                    .child(
                        Button::new("add-ssh")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Globe)
                            .tooltip("Add an SSH host")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_ssh_dialog(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("pair")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Network)
                            .tooltip("Pair through a relay")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.pair_dialog(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .id("agents-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        if self.tabs.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(div().text_xl().child("Loom"))
                .child("Pick an agent on the left, then open or create a session.")
                .child(div().text_sm().child(
                    "ctrl-n new session · ctrl-` new terminal · ctrl-b agents · ctrl-j terminals",
                ))
                .into_any_element();
        }
        let titles: Vec<String> = self.tabs.iter().map(|t| t.read(cx).title(cx)).collect();
        let active = self.tabs[self.active_tab].clone();
        v_flex()
            .size_full()
            .child(
                TabBar::new("session-tabs")
                    .selected_index(self.active_tab)
                    .on_click(cx.listener(|this, ix: &usize, window, cx| {
                        this.activate_tab(*ix, window, cx)
                    }))
                    .children(titles.into_iter().enumerate().map(|(ix, title)| {
                        Tab::new().label(title).suffix(
                            Button::new(("close-tab", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(ix, window, cx)
                                })),
                        )
                    })),
            )
            .child(div().flex_1().w_full().overflow_hidden().child(active))
            .into_any_element()
    }

    fn render_terminals(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let titles: Vec<String> = self
            .terminals
            .iter()
            .enumerate()
            .map(|(ix, t)| {
                let view = t.view.read(cx);
                let base = view
                    .title()
                    .unwrap_or_else(|| format!("Terminal {}", ix + 1));
                if view.exited().is_some() {
                    format!("{base} (exited)")
                } else {
                    base
                }
            })
            .collect();
        let active = self
            .terminals
            .get(self.active_terminal)
            .map(|t| t.view.clone());
        v_flex()
            .size_full()
            .border_t_1()
            .border_color(theme.border)
            .child(
                TabBar::new("terminal-tabs")
                    .selected_index(self.active_terminal)
                    .on_click(cx.listener(|this, ix: &usize, window, cx| {
                        this.active_terminal = *ix;
                        if let Some(t) = this.terminals.get(*ix) {
                            t.view.read(cx).focus_handle(cx).focus(window, cx);
                        }
                        cx.notify();
                    }))
                    .children(titles.into_iter().enumerate().map(|(ix, title)| {
                        Tab::new().label(title).suffix(
                            Button::new(("close-terminal", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.close_terminal(ix, cx)
                                })),
                        )
                    }))
                    .suffix(
                        Button::new("new-terminal")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("New terminal (ctrl-`)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_new_terminal(&NewTerminal, window, cx)
                            })),
                    ),
            )
            .child(match active {
                Some(view) => div()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .p_1()
                    .child(view)
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .p_3()
                    .text_color(theme.muted_foreground)
                    .child("No terminals. Press ctrl-` to open one on the active agent.")
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn render_status(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut left = h_flex().gap_3();
        let mut right = h_flex().gap_3();
        if let Some(conn) = &self.active_connection {
            let conn = conn.read(cx);
            let transport = match &conn.host {
                HostEntry::Local { .. } => "local socket",
                HostEntry::Ssh { .. } => "ssh",
                HostEntry::Relay { .. } => "relay (end-to-end encrypted)",
            };
            let status = match &conn.status {
                LinkStatus::Connected => "connected".to_owned(),
                LinkStatus::Disconnected(reason) => format!("disconnected: {reason}"),
            };
            left = left.child(conn.title()).child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(format!("{transport} · {status}")),
            );
        } else {
            left = left.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("not connected"),
            );
        }
        if let Some(tab) = self.active_session() {
            let conv = tab.read(cx).conversation.read(cx);
            if let Some(model) = conv
                .info
                .model
                .clone()
                .or_else(|| tab.read(cx).connection.read(cx).info.default_model.clone())
            {
                right = right.child(model);
            }
            if let Some(usage) = conv.last_usage {
                right = right.child(format!(
                    "context {} tokens · total {} in / {} out",
                    usage.input + usage.output,
                    conv.total_usage.input,
                    conv.total_usage.output
                ));
            }
            let pending = conv.pending_approvals().count();
            if pending > 0 {
                right = right.child(
                    div()
                        .text_color(theme.warning)
                        .child(format!("{pending} approval(s) pending")),
                );
            }
        }
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .justify_between()
            .text_xs()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .child(left)
            .child(right)
            .into_any_element()
    }
}

fn reconnect_target(conn: &AgentConnection) -> Target {
    match conn.host {
        HostEntry::Relay { .. } => Target::Instance(conn.info.instance.0.clone()),
        _ => Target::Attach(conn.info.workspace.clone()),
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .active_connection
            .as_ref()
            .map(|c| format!("Loom — {}", c.read(cx).title()))
            .unwrap_or_else(|| "Loom".into());
        window.set_window_title(&title);
        if window.focused(cx).is_none() {
            self.focus.focus(window, cx);
        }
        let theme = cx.theme().clone();
        let agents = self.render_agents_panel(cx);
        let center = self.render_center(cx);
        let terminals = self.render_terminals(cx);
        let status = self.render_status(cx);
        v_flex()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::on_toggle_left))
            .on_action(cx.listener(Self::on_toggle_bottom))
            .on_action(cx.listener(Self::on_new_session))
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(Self::on_providers))
            .on_action(cx.listener(Self::on_choose_model))
            .on_action(cx.listener(Self::on_add_ssh))
            .on_action(cx.listener(Self::on_pair))
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Loom"))
                        .when_some(self.active_connection.as_ref(), |el, conn| {
                            el.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(conn.read(cx).title()),
                            )
                        }),
                ),
            )
            .child(
                div().flex_1().w_full().overflow_hidden().child(
                    h_resizable("main")
                        .child(
                            resizable_panel()
                                .size(px(300.))
                                .size_range(px(200.)..px(600.))
                                .visible(self.show_left)
                                .child(agents),
                        )
                        .child(
                            resizable_panel().child(
                                v_resizable("center")
                                    .child(resizable_panel().child(center))
                                    .child(
                                        resizable_panel()
                                            .size(px(280.))
                                            .size_range(px(120.)..px(900.))
                                            .visible(self.show_bottom)
                                            .child(terminals),
                                    ),
                            ),
                        ),
                ),
            )
            .child(status)
    }
}
