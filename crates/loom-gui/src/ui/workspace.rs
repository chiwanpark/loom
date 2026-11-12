use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Action, AnyElement, AnyView, App, AppContext, AsyncWindowContext, ClickEvent, Context,
    DefiniteLength, DragMoveEvent, Entity, EntityId, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, Pixels, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, actions, div, img,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme, Icon, IconName, Selectable, Sizable, TitleBar, h_flex, h_resizable,
    resizable_panel, v_flex, v_resizable,
};
use loom_client::proto::{
    ChangeStatus, FileChange as GitChange, SessionId, SessionInfo, WorkspaceChanges,
};
use loom_config::{ClientConfig, HostEntry};

use crate::agent_list::{self, AgentEntry, Group, Link};
use crate::backend::Backend;
use crate::changes::{FileChange, display_path, session_changes};
use crate::connection::{self, AgentSummary, Env, Target};
use crate::conversation::{Activity, short_id, state_label};
use crate::layout::{self, GroupKey, SavedGroup, SavedLayout, SavedPane, SavedTab};
use crate::state::{AgentConnection, ConnectionEvent, LinkStatus, RemoteTerminal};
use crate::telemetry::Health;
use crate::term::{TerminalIo, TerminalView, mono_font};
use crate::ui::dialogs::{self, FormField, open_form};
use crate::ui::diff::DiffView;
use crate::ui::file_view::FileView;
use crate::ui::files::{FileTreeEvent, FileTreeView};
use crate::ui::notify;
use crate::ui::panes::{Axis, Member, PaneGroup, PaneId, SplitDirection};
use crate::ui::picker::{PickerItem, open_picker};
use crate::ui::session::SessionView;
use crate::ui::tab::Tab;
use crate::ui::tabs::{PanelId, TabList};

actions!(
    loom,
    [
        /// Shows the Agents tab of the right panel, or hides the panel when
        /// it is already showing. The agents list used to be a left panel.
        #[action(deprecated_aliases = ["loom::ToggleLeftDock"])]
        ToggleAgents,
        NewSession,
        NewTerminal,
        CloseTab,
        ArchiveSession,
        NextTab,
        PreviousTab,
        ShowProviders,
        ChooseModel,
        AddSshHost,
        PairRelay,
        Quit,
        SplitLeft,
        SplitRight,
        SplitUp,
        SplitDown,
        ActivatePaneLeft,
        ActivatePaneRight,
        ActivatePaneUp,
        ActivatePaneDown,
        ToggleRightDock,
        ToggleCommandPalette,
        FindSession,
        FindAgent,
        SelectTheme,
        RenameSession,
        ShowChanges,
        ShowFiles,
        /// Wraps long lines in the focused file or diff view, or stops
        /// wrapping them.
        ToggleLineWrap,
        /// Saves the focused file.
        Save
    ]
);

const CONTEXT: &str = "Workspace";
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const CHANGES_POLL: Duration = Duration::from_secs(2);
const RECONNECT_ATTEMPTS: usize = 20;
const OUTSIDE_TERMINAL: &str = "Workspace && !Terminal";
const DROP_TARGET_SIZE: f32 = 0.2;

pub fn bind_keys(overrides: &BTreeMap<String, String>, cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-b", ToggleAgents, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-alt-b", ToggleRightDock, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-e", ShowFiles, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-g", ShowChanges, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-p", ToggleCommandPalette, Some(CONTEXT)),
        KeyBinding::new("ctrl-p", FindSession, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-alt-o", FindAgent, Some(CONTEXT)),
        KeyBinding::new("ctrl-k ctrl-t", SelectTheme, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-n", NewSession, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-w", CloseTab, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-`", NewTerminal, Some(CONTEXT)),
        KeyBinding::new("ctrl-pagedown", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-pageup", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-m", ChooseModel, Some(CONTEXT)),
        KeyBinding::new("ctrl-k left", SplitLeft, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-k right", SplitRight, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-k up", SplitUp, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-k down", SplitDown, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-k ctrl-left", ActivatePaneLeft, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new(
            "ctrl-k ctrl-right",
            ActivatePaneRight,
            Some(OUTSIDE_TERMINAL),
        ),
        KeyBinding::new("ctrl-k ctrl-up", ActivatePaneUp, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-k ctrl-down", ActivatePaneDown, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-alt-left", ActivatePaneLeft, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-right", ActivatePaneRight, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-up", ActivatePaneUp, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-down", ActivatePaneDown, Some(CONTEXT)),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("alt-z", ToggleLineWrap, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-s", Save, Some(crate::ui::file_view::CONTEXT)),
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

/// What a row menu item does to the workspace.
type MenuAction = dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>);

/// An agent in the agents panel, with what it takes to act on it.
struct PanelAgent {
    entry: AgentEntry,
    host_ix: Option<usize>,
    instance: String,
    conn: Option<Entity<AgentConnection>>,
}

struct HostNode {
    entry: HostEntry,
    agents: Vec<AgentSummary>,
    loading: bool,
    error: Option<String>,
}

#[derive(Clone)]
enum Panel {
    Chat(Entity<SessionView>),
    Terminal(Entity<TerminalView>),
    Diff(Entity<DiffView>),
    File(Entity<FileView>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Chat,
    Terminal,
    Diff,
    File,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RightTab {
    Agents,
    Changes,
    Files,
}

impl Panel {
    fn kind(&self) -> PanelKind {
        match self {
            Panel::Chat(_) => PanelKind::Chat,
            Panel::Terminal(_) => PanelKind::Terminal,
            Panel::Diff(_) => PanelKind::Diff,
            Panel::File(_) => PanelKind::File,
        }
    }

    fn view(&self) -> AnyView {
        match self {
            Panel::Chat(view) => view.clone().into(),
            Panel::Terminal(view) => view.clone().into(),
            Panel::Diff(view) => view.clone().into(),
            Panel::File(view) => view.clone().into(),
        }
    }

    fn title(&self, cx: &App) -> String {
        match self {
            Panel::Chat(view) => view.read(cx).title(cx),
            Panel::Diff(view) => view.read(cx).title(),
            Panel::File(view) => view.read(cx).title(),
            Panel::Terminal(view) => {
                let view = view.read(cx);
                let base = view.title().unwrap_or_else(|| "Terminal".into());
                if view.exited().is_some() {
                    format!("{base} (exited)")
                } else {
                    base
                }
            }
        }
    }

    /// The file view, if this is one with unsaved changes.
    fn unsaved(&self, cx: &App) -> Option<&Entity<FileView>> {
        match self {
            Panel::File(view) if view.read(cx).is_dirty() => Some(view),
            _ => None,
        }
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        match self {
            Panel::Chat(view) => {
                let prompt = view.read(cx).prompt();
                prompt.update(cx, |input, cx| input.focus(window, cx));
            }
            Panel::Terminal(view) => view.read(cx).focus_handle(cx).focus(window, cx),
            Panel::Diff(view) => view.read(cx).focus_handle(cx).focus(window, cx),
            Panel::File(view) => view.read(cx).editor_focus(cx).focus(window, cx),
        }
    }
}

/// The last part of a slash-separated path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn palette_commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("workspace: new session", Box::new(NewSession)),
        ("workspace: new terminal", Box::new(NewTerminal)),
        ("workspace: close tab", Box::new(CloseTab)),
        ("workspace: next tab", Box::new(NextTab)),
        ("workspace: previous tab", Box::new(PreviousTab)),
        ("workspace: toggle agents", Box::new(ToggleAgents)),
        ("workspace: toggle right panel", Box::new(ToggleRightDock)),
        ("panel: changes", Box::new(ShowChanges)),
        ("panel: files", Box::new(ShowFiles)),
        ("view: toggle line wrap", Box::new(ToggleLineWrap)),
        ("file: save", Box::new(Save)),
        ("session: archive", Box::new(ArchiveSession)),
        ("session: rename", Box::new(RenameSession)),
        ("session: choose model", Box::new(ChooseModel)),
        ("finder: sessions", Box::new(FindSession)),
        ("finder: agents", Box::new(FindAgent)),
        ("pane: split left", Box::new(SplitLeft)),
        ("pane: split right", Box::new(SplitRight)),
        ("pane: split up", Box::new(SplitUp)),
        ("pane: split down", Box::new(SplitDown)),
        ("pane: activate left", Box::new(ActivatePaneLeft)),
        ("pane: activate right", Box::new(ActivatePaneRight)),
        ("pane: activate up", Box::new(ActivatePaneUp)),
        ("pane: activate down", Box::new(ActivatePaneDown)),
        ("theme: select theme", Box::new(SelectTheme)),
        ("agent: providers and sign-in", Box::new(ShowProviders)),
        ("hosts: add ssh host", Box::new(AddSshHost)),
        ("hosts: pair with a relay", Box::new(PairRelay)),
        ("app: quit", Box::new(Quit)),
    ]
}

/// How far to lift a line of text so that it looks centred beside an icon
/// of the same row.
///
/// gpui centres the font's ascent and descent in the line, so a font with a
/// tall ascent draws its letters low. The text is centred by the middle of
/// its capitals, moved a quarter of the way toward the middle of its
/// lowercase letters, as their bulk makes a word look lower. That weighting
/// suits the Linux UI fonts measured on screen; centring the capitals alone
/// left most of them a pixel low. The lift is whole device pixels, as glyphs
/// snap to them and would swallow a fraction.
fn text_lift(window: &Window, family: &SharedString, weight: FontWeight, size: Pixels) -> Pixels {
    let mut font = gpui::font(family.clone());
    font.weight = weight;
    let text = window.text_system();
    let id = text.resolve_font(&font);
    // Font metrics give the descent as negative, below the baseline.
    let descent = text.descent(id, size).abs();
    let ascent = text.ascent(id, size);
    let cap = text.cap_height(id, size);
    let x = text.x_height(id, size);
    // Old fonts, like DejaVu Sans, don't say how tall their letters are.
    // Rather than guess, leave the text where gpui puts it.
    if cap <= Pixels::ZERO || x <= Pixels::ZERO {
        return Pixels::ZERO;
    }
    let middle = (cap * 3. + x) / 8.;
    let lift = (ascent - descent) / 2. - middle;
    let scale = window.scale_factor();
    (lift * scale).round() / scale
}

fn panel_icon(kind: PanelKind) -> Icon {
    match kind {
        PanelKind::Chat => Icon::new(IconName::Bot).xsmall(),
        PanelKind::Terminal => Icon::new(IconName::SquareTerminal).xsmall(),
        PanelKind::Diff => Icon::new(gpui_kit_assets::IconName::FileDiff).xsmall(),
        PanelKind::File => Icon::new(IconName::FileText).xsmall(),
    }
}

struct Pane {
    id: PaneId,
    tabs: TabList<Panel>,
}

struct SessionGroup {
    connection: Entity<AgentConnection>,
    session: SessionId,
    panes: Vec<Pane>,
    layout: PaneGroup,
    active_pane: PaneId,
}

impl SessionGroup {
    fn new(connection: Entity<AgentConnection>, session: SessionId, pane: PaneId) -> Self {
        Self {
            connection,
            session,
            panes: vec![Pane {
                id: pane,
                tabs: TabList::default(),
            }],
            layout: PaneGroup::new(pane),
            active_pane: pane,
        }
    }

    fn chat(&self) -> Option<Entity<SessionView>> {
        self.panes
            .iter()
            .flat_map(|pane| pane.tabs.iter())
            .find_map(|(_, panel)| match panel {
                Panel::Chat(view) => Some(view.clone()),
                Panel::Terminal(_) | Panel::Diff(_) | Panel::File(_) => None,
            })
    }

    fn find_diff(&self, path: &PathBuf, git: bool, cx: &App) -> Option<(PaneId, usize)> {
        self.panes.iter().find_map(|pane| {
            pane.tabs
                .find(|panel| {
                    matches!(panel, Panel::Diff(view)
                        if view.read(cx).path() == path && view.read(cx).is_git() == git)
                })
                .map(|ix| (pane.id, ix))
        })
    }

    fn is(&self, conn: &Entity<AgentConnection>, session: &SessionId) -> bool {
        &self.connection == conn && &self.session == session
    }

    fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    fn active(&self) -> Option<&Pane> {
        self.pane(self.active_pane)
    }

    fn active_mut(&mut self) -> Option<&mut Pane> {
        let id = self.active_pane;
        self.pane_mut(id)
    }

    fn active_panel(&self) -> Option<&Panel> {
        self.active().and_then(|pane| pane.tabs.active())
    }

    fn is_empty(&self) -> bool {
        self.panes.iter().all(|pane| pane.tabs.is_empty())
    }

    fn add_pane(&mut self, target: PaneId, new: PaneId, direction: SplitDirection) {
        self.layout.split(target, new, direction);
        self.panes.push(Pane {
            id: new,
            tabs: TabList::default(),
        });
        self.active_pane = new;
    }

    fn remove_pane(&mut self, id: PaneId) -> bool {
        if self.panes.len() <= 1 || !self.layout.remove(id) {
            return false;
        }
        self.panes.retain(|pane| pane.id != id);
        if self.active_pane == id {
            self.active_pane = self.layout.first_pane();
        }
        true
    }

    fn take_panel(&mut self, pane: PaneId, panel: PanelId) -> Option<Panel> {
        let source = self.pane_mut(pane)?;
        let ix = source.tabs.position(panel)?;
        let item = source.tabs.remove(ix)?;
        if source.tabs.is_empty() {
            self.remove_pane(pane);
        }
        Some(item)
    }

    fn drain(&mut self) -> Vec<Panel> {
        self.panes
            .iter_mut()
            .flat_map(|pane| pane.tabs.drain())
            .collect()
    }
}

#[derive(Clone)]
struct DraggedTab {
    pane: PaneId,
    panel: PanelId,
    ix: usize,
    kind: PanelKind,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().font_family(cx.theme().font_family.clone()).child(
            Tab::new("dragged-tab")
                .selected(true)
                .start_slot(panel_icon(self.kind))
                .child(self.title.clone()),
        )
    }
}

pub struct Workspace {
    backend: Backend,
    env: Env,
    config: ClientConfig,
    config_path: PathBuf,
    hosts: Vec<HostNode>,
    connections: Vec<Entity<AgentConnection>>,
    connecting: HashSet<(String, String)>,
    /// Why the last connection to an agent failed, by `(host, instance)`.
    connect_errors: HashMap<(String, String), String>,
    /// Agents stopped from here, which may still be listed while they exit.
    stopped: HashSet<(String, String)>,
    show_archived: bool,
    active_connection: Option<Entity<AgentConnection>>,
    groups: Vec<SessionGroup>,
    active_group: Option<usize>,
    drag_split: Option<(PaneId, Option<SplitDirection>)>,
    next_panel: PanelId,
    show_right: bool,
    changes_cache: Option<(EntityId, u64, Rc<Vec<FileChange>>)>,
    right_tab: RightTab,
    file_trees: Vec<Entity<FileTreeView>>,
    poll_ticks: u64,
    focus: FocusHandle,
    subscriptions: Vec<Subscription>,
    layout_path: PathBuf,
    /// Saved tab groups whose agent has not listed its sessions yet.
    pending_layout: Vec<SavedGroup>,
    /// The saved active group, until it is restored or another is opened.
    pending_active: Option<GroupKey>,
    /// What was last read or written, so unchanged layouts are not rewritten.
    saved_layout: SavedLayout,
    /// Restored terminal tabs still waiting for the agent, with what they
    /// were saved as.
    restoring: HashMap<EntityId, SavedTab>,
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
            })
            .collect();
        let layout_path = layout::path(&env.dirs.state_dir());
        let mut saved_layout = layout::load(&layout_path);
        // Groups on hosts that were removed can never be restored.
        saved_layout
            .groups
            .retain(|g| config.hosts.iter().any(|h| h.name() == g.key.host));
        let mut this = Self {
            backend,
            env,
            config,
            config_path,
            hosts,
            connections: Vec::new(),
            connecting: HashSet::new(),
            connect_errors: HashMap::new(),
            stopped: HashSet::new(),
            show_archived: false,
            active_connection: None,
            groups: Vec::new(),
            active_group: None,
            drag_split: None,
            next_panel: 0,
            show_right: true,
            changes_cache: None,
            right_tab: RightTab::Agents,
            file_trees: Vec::new(),
            poll_ticks: 0,
            focus: cx.focus_handle(),
            subscriptions: Vec::new(),
            layout_path,
            pending_layout: saved_layout.groups.clone(),
            pending_active: saved_layout.active.clone(),
            saved_layout,
            restoring: HashMap::new(),
        };
        cx.on_app_quit(|this, cx| {
            this.save_layout(cx);
            async {}
        })
        .detach();
        cx.on_release(|this, cx| this.save_layout(cx)).detach();
        let weak = cx.entity().downgrade();
        let handle = window.window_handle();
        App::on_action(cx, move |_: &Quit, cx: &mut App| {
            // The window is busy dispatching the key press that quits; ask
            // once it is free again.
            let weak = weak.clone();
            cx.defer(move |cx| {
                let asked = handle
                    .update(cx, |_, window, cx| {
                        weak.update(cx, |ws, cx| ws.quit(window, cx)).is_ok()
                    })
                    .unwrap_or(false);
                if !asked {
                    cx.quit();
                }
            });
        });
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |ws, cx| {
                if ws.unsaved_in(|_| true, cx).is_empty() {
                    true
                } else {
                    ws.close_window(window, cx);
                    false
                }
            })
            .unwrap_or(true)
        });
        for ix in 0..this.hosts.len() {
            this.refresh_host(ix, window, cx);
        }
        this.focus.focus(window, cx);
        cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            loop {
                cx.background_executor().timer(CHANGES_POLL).await;
                let polled = this.update(cx, |ws, cx| {
                    ws.poll_changes(cx);
                    ws.save_layout(cx);
                });
                if polled.is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn find_file_tree(
        &self,
        conn: &Entity<AgentConnection>,
        cx: &App,
    ) -> Option<Entity<FileTreeView>> {
        self.file_trees
            .iter()
            .find(|tree| tree.read(cx).connection() == conn)
            .cloned()
    }

    fn file_tree(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<FileTreeView> {
        if let Some(tree) = self.find_file_tree(conn, cx) {
            return tree;
        }
        let tree = cx.new(|cx| FileTreeView::new(conn.clone(), cx));
        let tree_conn = conn.clone();
        self.subscriptions.push(cx.subscribe_in(
            &tree,
            window,
            move |this, _, event: &FileTreeEvent, window, cx| match event {
                FileTreeEvent::Open(path) => {
                    this.open_file(tree_conn.clone(), path.clone(), window, cx)
                }
            },
        ));
        self.file_trees.push(tree.clone());
        tree
    }

    fn open_file(
        &mut self,
        conn: Entity<AgentConnection>,
        relative: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_group().is_none() {
            notify::info(window, cx, "Open a session to view files.");
            return;
        }
        let path = conn.read(cx).info.workspace.join(&relative);
        let found = self.active_group().and_then(|group| {
            group.panes.iter().find_map(|pane| {
                pane.tabs
                    .find(
                        |panel| matches!(panel, Panel::File(view) if view.read(cx).path() == &path),
                    )
                    .map(|ix| (pane.id, ix))
            })
        });
        if let Some((pane, ix)) = found {
            self.activate_tab(pane, ix, window, cx);
            if let Some(Panel::File(view)) = self.active_group().and_then(|g| g.active_panel()) {
                view.update(cx, |view, cx| view.reload(window, cx));
            }
            return;
        }
        let wrap = self.config.appearance.wrap_lines;
        let view = cx.new(|cx| FileView::new(conn, relative, wrap, window, cx));
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        let id = self.alloc_panel();
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut()) {
            pane.tabs.add(id, Panel::File(view), None);
        }
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    fn on_show_changes(&mut self, _: &ShowChanges, _: &mut Window, cx: &mut Context<Self>) {
        self.show_right = true;
        self.right_tab = RightTab::Changes;
        cx.notify();
    }

    fn on_show_files(&mut self, _: &ShowFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.show_right = true;
        self.right_tab = RightTab::Files;
        if let Some(conn) = self.changes_connection() {
            let tree = self.file_tree(&conn, window, cx);
            tree.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn changes_connection(&self) -> Option<Entity<AgentConnection>> {
        self.active_group()
            .map(|g| g.connection.clone())
            .or_else(|| self.active_connection.clone())
    }

    fn poll_changes(&mut self, cx: &mut Context<Self>) {
        self.poll_ticks += 1;
        if let Some(conn) = self.active_connection.clone() {
            conn.update(cx, |conn, cx| conn.refresh_telemetry(false, cx));
        }
        if self.show_right
            && let Some(conn) = self.changes_connection()
        {
            conn.update(cx, |conn, cx| conn.refresh_changes(cx));
            if self.right_tab == RightTab::Files
                && self.poll_ticks.is_multiple_of(5)
                && let Some(tree) = self.find_file_tree(&conn, cx)
            {
                tree.update(cx, |tree, cx| tree.refresh(cx));
            }
        }
        if let Some(chat) = self.active_group().and_then(SessionGroup::chat)
            && let Some((provider, marker)) = chat.read(cx).usage_source(cx)
        {
            let conn = chat.read(cx).connection.clone();
            conn.update(cx, |conn, cx| conn.refresh_usage(&provider, marker, cx));
        }
        let Some(group) = self.active_group() else {
            return;
        };
        let views: Vec<Entity<DiffView>> = group
            .panes
            .iter()
            .filter_map(|pane| match pane.tabs.active() {
                Some(Panel::Diff(view)) if view.read(cx).is_git() => Some(view.clone()),
                _ => None,
            })
            .collect();
        for view in views {
            view.update(cx, |view, cx| view.reload(cx));
        }
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
        self.list_host(ix, true, window, cx);
    }

    /// Lists the host's agents again. With `retry_failed`, agents whose
    /// connection failed are tried again; otherwise they wait for a click, so
    /// a failure that triggers a listing cannot loop.
    fn list_host(
        &mut self,
        ix: usize,
        retry_failed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.hosts.get_mut(ix) else {
            return;
        };
        host.loading = true;
        host.error = None;
        let entry = host.entry.clone();
        if retry_failed {
            self.connect_errors
                .retain(|(host, _), _| host != entry.name());
        }
        let env = self.env.clone();
        let task = self
            .backend
            .run(async move { connection::list_agents(&env, &entry).await });
        let name = host.entry.name().to_owned();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(ix) = this.hosts.iter().position(|h| h.entry.name() == name) {
                    let host = &mut this.hosts[ix];
                    host.loading = false;
                    match result {
                        Some(Ok(agents)) => {
                            host.agents = agents;
                            this.connect_discovered(ix, window, cx);
                        }
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

    /// Connects in the background to the host's agents that have no
    /// connection yet, so the agents panel can list their sessions and follow
    /// their state. Agents whose last connection failed wait for a click.
    fn connect_discovered(&mut self, host_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(host_ix) else {
            return;
        };
        let name = host.entry.name().to_owned();
        let pending: Vec<String> = host
            .agents
            .iter()
            .map(|agent| agent.instance.clone())
            .filter(|instance| {
                let key = (name.clone(), instance.clone());
                self.find_connection(&name, instance, cx).is_none()
                    && !self.connect_errors.contains_key(&key)
                    && !self.stopped.contains(&key)
            })
            .collect();
        for instance in pending {
            self.connect_with(host_ix, Target::Instance(instance), true, window, cx);
        }
    }

    /// Drops an agent from its host's list until the host is listed again.
    fn forget_agent(&mut self, host: &str, instance: &str) {
        if let Some(entry) = self.hosts.iter_mut().find(|h| h.entry.name() == host) {
            entry.agents.retain(|agent| agent.instance != instance);
        }
        self.connect_errors
            .remove(&(host.to_owned(), instance.to_owned()));
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
        self.connect_with(host_ix, target, false, window, cx);
    }

    /// Connects to an agent. A `background` connection neither reports
    /// errors nor replaces the active agent.
    fn connect_with(
        &mut self,
        host_ix: usize,
        target: Target,
        background: bool,
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
        let by_instance = matches!(target, Target::Instance(_));
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
                        this.connect_errors.remove(&key);
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
                        // A restarted agent takes over the old one's connection.
                        let replaced = existing
                            .as_ref()
                            .map(|c| c.read(cx).info.instance.0.clone())
                            .filter(|old| *old != instance);
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
                        if !background || this.active_connection.is_none() {
                            this.active_connection = Some(conn);
                        }
                        if let Some(old) = &replaced {
                            this.forget_agent(entry.name(), old);
                        }
                        if (!background || replaced.is_some())
                            && let Some(ix) = this.hosts.iter().position(|h| h.entry == entry)
                        {
                            this.list_host(ix, !background, window, cx);
                        }
                    }
                    Some(Err(err)) => {
                        if background {
                            tracing::info!("background connection failed: {err}");
                        } else {
                            notify::error(window, cx, err.clone());
                        }
                        this.connect_errors.insert(key, err);
                        // The agent may be gone; the listing drops it if so.
                        if by_instance
                            && let Some(ix) = this.hosts.iter().position(|h| h.entry == entry)
                        {
                            this.list_host(ix, false, window, cx);
                        }
                    }
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
                            this.connect_with(ix, target, true, window, cx);
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
                ConnectionEvent::Failed(message) => notify::error(window, cx, message.clone()),
                ConnectionEvent::SessionRemoved(id) => {
                    let conn = conn.clone();
                    let id = id.clone();
                    this.remove_groups(window, cx, |g| g.is(&conn, &id));
                }
                ConnectionEvent::SessionsLoaded => this.restore_groups(conn, window, cx),
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

    /// How a tab is opened again after a restart, or `None` when it is not
    /// worth keeping (an ended terminal).
    fn saved_tab(&self, group: &SessionGroup, panel: &Panel, cx: &App) -> Option<SavedTab> {
        match panel {
            Panel::Chat(_) => Some(SavedTab::Conversation),
            Panel::Terminal(view) => {
                if let Some(saved) = self.restoring.get(&view.entity_id()) {
                    return Some(saved.clone());
                }
                if view.read(cx).exited().is_some() {
                    return None;
                }
                let conn = group.connection.read(cx);
                let id = conn.terminal_id(view)?;
                Some(SavedTab::Terminal {
                    instance: conn.info.instance.0.clone(),
                    id,
                })
            }
            Panel::Diff(view) => Some(view.read(cx).saved_tab()),
            Panel::File(view) => Some(SavedTab::File {
                path: view.read(cx).relative().to_owned(),
            }),
        }
    }

    fn group_key(group: &SessionGroup, cx: &App) -> GroupKey {
        let conn = group.connection.read(cx);
        GroupKey {
            host: conn.host.name().to_owned(),
            workspace: conn.info.workspace.clone(),
            session: group.session.0.clone(),
        }
    }

    fn saved_group(&self, group: &SessionGroup, cx: &App) -> Option<SavedGroup> {
        let mut order = Vec::new();
        let root = layout::save_member(&group.layout.root, &mut |id| {
            let pane = group.pane(id)?;
            let saved = SavedPane::keep(
                pane.tabs
                    .iter()
                    .map(|(_, panel)| self.saved_tab(group, panel, cx)),
                pane.tabs.active_index(),
            )?;
            order.push(id);
            Some(saved)
        })?;
        Some(SavedGroup {
            key: Self::group_key(group, cx),
            root,
            active_pane: order
                .iter()
                .position(|id| *id == group.active_pane)
                .unwrap_or(0),
        })
    }

    /// The open tab groups, followed by the saved ones not restored yet.
    fn layout_snapshot(&self, cx: &App) -> SavedLayout {
        let mut groups: Vec<SavedGroup> = self
            .groups
            .iter()
            .filter_map(|group| self.saved_group(group, cx))
            .collect();
        for pending in &self.pending_layout {
            if !groups.iter().any(|g| g.key == pending.key) {
                groups.push(pending.clone());
            }
        }
        let active = match self.active_group() {
            Some(group) => Some(Self::group_key(group, cx)),
            None => self.pending_active.clone(),
        };
        SavedLayout {
            groups,
            active,
            ..SavedLayout::default()
        }
    }

    fn save_layout(&mut self, cx: &App) {
        let layout = self.layout_snapshot(cx);
        if layout == self.saved_layout {
            return;
        }
        if let Err(err) = layout::save(&self.layout_path, &layout) {
            tracing::warn!("cannot save {}: {err}", self.layout_path.display());
        }
        self.saved_layout = layout;
    }

    /// Reopens the saved tab groups of an agent's workspace once it has
    /// listed its sessions. Groups of sessions that are gone or archived are
    /// dropped.
    fn restore_groups(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = conn.read(cx);
        let host = read.host.name().to_owned();
        let workspace = read.info.workspace.clone();
        let sessions = read.sessions.clone();
        let mut ix = 0;
        while ix < self.pending_layout.len() {
            let key = &self.pending_layout[ix].key;
            if key.host != host || key.workspace != workspace {
                ix += 1;
                continue;
            }
            let saved = self.pending_layout.remove(ix);
            let active = self.pending_active.as_ref() == Some(&saved.key);
            if active {
                self.pending_active = None;
            }
            let Some(info) = sessions
                .iter()
                .find(|s| s.id.0 == saved.key.session && !s.archived)
            else {
                continue;
            };
            if self.groups.iter().any(|g| g.is(conn, &info.id)) {
                continue;
            }
            if let Some(group) = self.restore_group(conn, info, &saved, window, cx) {
                self.groups.push(group);
                if active && self.active_group.is_none() {
                    self.activate_group(self.groups.len() - 1, window, cx);
                }
            }
        }
        cx.notify();
    }

    fn restore_group(
        &mut self,
        conn: &Entity<AgentConnection>,
        info: &SessionInfo,
        saved: &SavedGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<SessionGroup> {
        let mut saved_panes = Vec::new();
        let root =
            layout::restore_member(&saved.root, &mut || self.alloc_panel(), &mut saved_panes)?;
        let has_chat = saved_panes
            .iter()
            .any(|(_, pane)| pane.tabs.contains(&SavedTab::Conversation));
        let mut panes = Vec::new();
        for (id, pane) in saved_panes {
            let mut tabs = TabList::default();
            for tab in &pane.tabs {
                if let Some(panel) = self.restore_panel(conn, info, tab, has_chat, window, cx) {
                    let panel_id = self.alloc_panel();
                    tabs.add(panel_id, panel, Some(tabs.len()));
                }
            }
            tabs.activate(pane.active.min(tabs.len().saturating_sub(1)));
            panes.push(Pane { id, tabs });
        }
        let order: Vec<PaneId> = panes.iter().map(|p| p.id).collect();
        let mut group = SessionGroup {
            connection: conn.clone(),
            session: info.id.clone(),
            layout: PaneGroup::from_root(root),
            active_pane: order.get(saved.active_pane).copied().unwrap_or(order[0]),
            panes,
        };
        let empty: Vec<PaneId> = group
            .panes
            .iter()
            .filter(|p| p.tabs.is_empty())
            .map(|p| p.id)
            .collect();
        for id in empty {
            group.remove_pane(id);
        }
        (!group.is_empty()).then_some(group)
    }

    fn restore_panel(
        &mut self,
        conn: &Entity<AgentConnection>,
        info: &SessionInfo,
        tab: &SavedTab,
        has_chat: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Panel> {
        let wrap = self.config.appearance.wrap_lines;
        let panel = match tab {
            SavedTab::Conversation => return Some(self.chat_panel(conn, info, window, cx)),
            SavedTab::Terminal { instance, id } => {
                return Some(Panel::Terminal(
                    self.restore_terminal(conn, instance, *id, window, cx),
                ));
            }
            SavedTab::GitDiff { path, old_path } => {
                let view = cx.new(|cx| {
                    DiffView::git(conn.clone(), path.clone(), old_path.clone(), wrap, cx)
                });
                self.subscriptions
                    .push(cx.observe(&view, |_, _, cx| cx.notify()));
                Panel::Diff(view)
            }
            // A session diff follows the conversation, which only a
            // conversation tab keeps open.
            SavedTab::SessionDiff { .. } if !has_chat => return None,
            SavedTab::SessionDiff { path } => {
                let conversation = conn.update(cx, |conn, cx| conn.conversation(info, cx));
                let title = display_path(path, &conn.read(cx).info.workspace);
                let view =
                    cx.new(|cx| DiffView::session(conversation, path.clone(), title, wrap, cx));
                self.subscriptions
                    .push(cx.observe(&view, |_, _, cx| cx.notify()));
                Panel::Diff(view)
            }
            SavedTab::File { path } => {
                let view = cx.new(|cx| FileView::new(conn.clone(), path.clone(), wrap, window, cx));
                self.subscriptions
                    .push(cx.observe(&view, |_, _, cx| cx.notify()));
                Panel::File(view)
            }
        };
        Some(panel)
    }

    /// A terminal tab that reattaches to its terminal when the same agent
    /// still runs it, and opens a new one in its place otherwise.
    fn restore_terminal(
        &mut self,
        conn: &Entity<AgentConnection>,
        instance: &str,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let appearance = &self.config.appearance;
        let font = mono_font(
            appearance.mono_font.as_deref(),
            &appearance.mono_font_fallbacks,
            cx,
        );
        let size = px(appearance.mono_font_size);
        let view = cx.new(|cx| TerminalView::new(None, font, size, cx));
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        self.restoring.insert(
            view.entity_id(),
            SavedTab::Terminal {
                instance: instance.to_owned(),
                id,
            },
        );
        let read = conn.read(cx);
        let client = read.client.clone();
        let backend = read.backend.clone();
        let (cols, rows) = view.read(cx).grid_size();
        // Register before attaching, so the output that the redraw after
        // attaching produces reaches the view.
        let existing = (read.info.instance.0 == instance).then(|| {
            let remote = RemoteTerminal::new(&backend, client.clone(), id);
            let rebind = remote.rebinder();
            conn.update(cx, |conn, _| conn.register_terminal(id, &view, rebind));
            Rc::new(remote) as Rc<dyn TerminalIo>
        });
        let attach = existing.is_some().then_some(id);
        let task = backend.run({
            let client = client.clone();
            async move {
                if let Some(id) = attach {
                    match client.attach_terminal(id, cols, rows).await {
                        Ok(()) => return Ok(None),
                        Err(err) => tracing::debug!(terminal = id, "cannot reattach: {err}"),
                    }
                }
                client.open_terminal(cols, rows).await.map(Some)
            }
        });
        let conn = conn.clone();
        let view_handle = view.clone();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.restoring.remove(&view_handle.entity_id());
                let io = match result {
                    Some(Ok(None)) => existing,
                    Some(Ok(Some(new_id))) => {
                        if existing.is_some() {
                            conn.update(cx, |conn, _| conn.unregister_terminal(id));
                        }
                        let remote = RemoteTerminal::new(&backend, client, new_id);
                        let rebind = remote.rebinder();
                        conn.update(cx, |conn, _| {
                            conn.register_terminal(new_id, &view_handle, rebind)
                        });
                        Some(Rc::new(remote) as Rc<dyn TerminalIo>)
                    }
                    Some(Err(err)) => {
                        if existing.is_some() {
                            conn.update(cx, |conn, _| conn.unregister_terminal(id));
                        }
                        let reason = format!("cannot open a terminal: {err}");
                        view_handle.update(cx, |view, cx| view.set_lost(&reason, cx));
                        None
                    }
                    None => None,
                };
                if let Some(io) = io {
                    view_handle.update(cx, |view, cx| view.connect(io, cx));
                }
                cx.notify();
            });
        })
        .detach();
        view
    }

    fn alloc_panel(&mut self) -> PanelId {
        self.next_panel += 1;
        self.next_panel
    }

    fn chat_panel(
        &mut self,
        conn: &Entity<AgentConnection>,
        info: &SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Panel {
        let conversation = conn.update(cx, |conn, cx| conn.conversation(info, cx));
        let view = cx.new(|cx| SessionView::new(conn.clone(), conversation, window, cx));
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        Panel::Chat(view)
    }

    fn open_session(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = match self.groups.iter().position(|g| g.is(&conn, &info.id)) {
            Some(ix) => ix,
            None => {
                let pane = self.alloc_panel();
                self.groups
                    .push(SessionGroup::new(conn.clone(), info.id.clone(), pane));
                self.groups.len() - 1
            }
        };
        if self.groups[ix].chat().is_none() {
            let panel = self.chat_panel(&conn, &info, window, cx);
            let id = self.alloc_panel();
            if let Some(pane) = self.groups[ix].active_mut() {
                pane.tabs.add(id, panel, Some(0));
            }
        }
        self.activate_group(ix, window, cx);
    }

    fn active_group(&self) -> Option<&SessionGroup> {
        self.active_group.and_then(|ix| self.groups.get(ix))
    }

    fn activate_group(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.groups.get(ix) else {
            return;
        };
        self.active_group = Some(ix);
        self.active_connection = Some(group.connection.clone());
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    fn focus_active_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.active_group().and_then(|g| g.active_panel()).cloned() {
            panel.focus(window, cx);
        }
    }

    fn active_group_mut(&mut self) -> Option<&mut SessionGroup> {
        self.active_group.and_then(|g| self.groups.get_mut(g))
    }

    fn set_active_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group_mut()
            && group.active_pane != pane
            && group.pane(pane).is_some()
        {
            group.active_pane = pane;
            cx.notify();
        }
    }

    fn activate_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        self.set_active_pane(pane, cx);
        self.focus_active_panel(window, cx);
    }

    fn activate_tab(
        &mut self,
        pane: PaneId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self.active_group_mut()
            && let Some(target) = group.pane_mut(pane)
            && target.tabs.activate(ix)
        {
            group.active_pane = pane;
            self.focus_active_panel(window, cx);
            cx.notify();
        }
    }

    fn release_panel(group: &SessionGroup, panel: &Panel, cx: &mut Context<Self>) {
        if let Panel::Chat(_) = panel {
            let id = group.session.clone();
            group
                .connection
                .update(cx, |conn, _| conn.release_conversation(&id));
        }
    }

    /// Closes a tab of the active group, after asking what to do with its
    /// unsaved changes, if it has any.
    fn close_tab(&mut self, pane: PaneId, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, panel)) = self
            .active_group()
            .and_then(|g| g.pane(pane))
            .and_then(|p| p.tabs.iter().nth(ix))
        else {
            return;
        };
        let unsaved = panel.unsaved(cx).cloned().into_iter().collect();
        self.guard_unsaved(unsaved, window, cx, move |ws, window, cx| {
            ws.close_panel(id, window, cx)
        });
    }

    /// Closes a tab wherever it is now; it may have moved while a dialog
    /// asked about it.
    fn close_panel(&mut self, id: PanelId, window: &mut Window, cx: &mut Context<Self>) {
        let Some((gix, pane, ix)) = self.groups.iter().enumerate().find_map(|(gix, g)| {
            g.panes
                .iter()
                .find_map(|p| p.tabs.position(id).map(|ix| (gix, p.id, ix)))
        }) else {
            return;
        };
        let group = &mut self.groups[gix];
        let Some(target) = group.pane_mut(pane) else {
            return;
        };
        let Some(panel) = target.tabs.remove(ix) else {
            return;
        };
        if target.tabs.is_empty() {
            group.remove_pane(pane);
        }
        let empty = group.is_empty();
        let group = &self.groups[gix];
        Self::release_panel(group, &panel, cx);
        if empty {
            self.remove_group(gix, window, cx);
        } else {
            self.focus_active_panel(window, cx);
        }
        cx.notify();
    }

    /// File views with unsaved changes in the groups that match.
    fn unsaved_in(&self, pred: impl Fn(&SessionGroup) -> bool, cx: &App) -> Vec<Entity<FileView>> {
        self.groups
            .iter()
            .filter(|g| pred(g))
            .flat_map(|g| g.panes.iter())
            .flat_map(|p| p.tabs.iter())
            .filter_map(|(_, panel)| panel.unsaved(cx).cloned())
            .collect()
    }

    /// Runs `then` once the unsaved changes in `views` are taken care of:
    /// right away when there are none, and otherwise after asking, once
    /// they are saved or the user chose to throw them away. Cancelling, or
    /// a save that fails, leaves everything as it was.
    fn guard_unsaved(
        &mut self,
        views: Vec<Entity<FileView>>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if views.is_empty() {
            then(self, window, cx);
            return;
        }
        let names: Vec<String> = views
            .iter()
            .map(|v| v.read(cx).relative().to_string())
            .collect();
        let (title, description) = match names.as_slice() {
            [one] => (
                format!("Save your changes to {}?", file_name(one)),
                format!("{one} has changes that are not saved."),
            ),
            many => (
                format!("Save your changes to {} files?", many.len()),
                format!(
                    "These files have changes that are not saved: {}.",
                    many.join(", ")
                ),
            ),
        };
        let then = Rc::new(std::cell::RefCell::new(Some(then)));
        let this = cx.entity().downgrade();
        let discard = {
            let (this, then) = (this.clone(), then.clone());
            move |window: &mut Window, cx: &mut App| {
                if let Some(then) = then.borrow_mut().take() {
                    let _ = this.update(cx, |ws, cx| then(ws, window, cx));
                }
            }
        };
        let save = move |window: &mut Window, cx: &mut App| {
            let saves: Vec<_> = views
                .iter()
                .map(|v| v.update(cx, |v, cx| v.save(window, cx)))
                .collect();
            let (this, then) = (this.clone(), then.clone());
            window
                .spawn(cx, async move |cx: &mut AsyncWindowContext| {
                    let saved = futures::future::join_all(saves).await;
                    if saved.into_iter().all(|ok| ok)
                        && let Some(then) = then.borrow_mut().take()
                    {
                        let _ = this.update_in(cx, |ws, window, cx| then(ws, window, cx));
                    }
                })
                .detach();
        };
        dialogs::unsaved_changes(title, description, save, discard, window, cx);
    }

    /// Ctrl+Q: quits once unsaved changes are saved or thrown away.
    fn quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.unsaved_in(|_| true, cx);
        self.guard_unsaved(unsaved, window, cx, |_, _, cx| cx.quit());
    }

    /// The title bar's close button, or a close request from the window
    /// manager while there are unsaved changes.
    fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.unsaved_in(|_| true, cx);
        self.guard_unsaved(unsaved, window, cx, |_, window, _| window.remove_window());
    }

    fn remove_group(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.groups.len() {
            return;
        }
        let mut group = self.groups.remove(ix);
        for panel in group.drain() {
            Self::release_panel(&group, &panel, cx);
        }
        self.active_group = match self.active_group {
            Some(active) if active == ix => None,
            Some(active) if active > ix => Some(active - 1),
            other => other,
        };
        if self.active_group.is_none() {
            self.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn remove_groups(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        pred: impl Fn(&SessionGroup) -> bool,
    ) {
        while let Some(ix) = self.groups.iter().position(&pred) {
            self.remove_group(ix, window, cx);
        }
    }

    fn handle_tab_drop(
        &mut self,
        dragged: &DraggedTab,
        pane: PaneId,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_split = None;
        let Some(group) = self.active_group_mut() else {
            return;
        };
        if group.pane(pane).is_none() {
            return;
        }
        if dragged.pane == pane {
            if let Some(target) = group.pane_mut(pane) {
                target.tabs.move_item(dragged.panel, ix);
            }
        } else {
            let Some(panel) = group.take_panel(dragged.pane, dragged.panel) else {
                return;
            };
            if let Some(target) = group.pane_mut(pane) {
                target.tabs.add(dragged.panel, panel, Some(ix));
            }
        }
        group.active_pane = pane;
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    fn handle_drag_move(
        &mut self,
        pane: PaneId,
        event: &DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        if !event.bounds.contains(&event.event.position) {
            return;
        }
        let relative = event.event.position - event.bounds.origin;
        let direction = SplitDirection::from_drop(
            f32::from(relative.x),
            f32::from(relative.y),
            f32::from(event.bounds.size.width),
            f32::from(event.bounds.size.height),
            DROP_TARGET_SIZE,
        );
        let split = Some((pane, direction));
        if self.drag_split != split {
            self.drag_split = split;
            cx.notify();
        }
    }

    fn handle_split_drop(
        &mut self,
        dragged: &DraggedTab,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let direction = match self.drag_split.take() {
            Some((target, direction)) if target == pane => direction,
            _ => None,
        };
        let Some(direction) = direction else {
            let len = self
                .active_group()
                .and_then(|g| g.pane(pane))
                .map(|p| p.tabs.len())
                .unwrap_or(0);
            self.handle_tab_drop(dragged, pane, len, window, cx);
            return;
        };
        self.split_with(dragged.pane, dragged.panel, pane, direction, window, cx);
    }

    fn split_with(
        &mut self,
        source: PaneId,
        panel: PanelId,
        target: PaneId,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_pane = self.alloc_panel();
        let Some(group) = self.active_group_mut() else {
            return;
        };
        let lone = source == target && group.pane(source).is_some_and(|p| p.tabs.len() <= 1);
        if lone || group.pane(target).is_none() {
            return;
        }
        let Some(item) = group.take_panel(source, panel) else {
            return;
        };
        group.add_pane(target, new_pane, direction);
        if let Some(pane) = group.pane_mut(new_pane) {
            pane.tabs.add(panel, item, None);
        }
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    fn split_active(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            return;
        };
        let Some(pane) = group.active() else {
            return;
        };
        let active = pane.tabs.active_index();
        let panel = pane.tabs.iter().nth(active).map(|(id, _)| id);
        match panel {
            Some(panel) if pane.tabs.len() > 1 => {
                let id = pane.id;
                self.split_with(id, panel, id, direction, window, cx)
            }
            _ => self.new_terminal(Some(direction), window, cx),
        }
    }

    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .active_group()
            .and_then(|g| g.layout.find_pane_in_direction(g.active_pane, direction));
        if let Some(target) = target {
            self.activate_pane(target, window, cx);
        }
    }

    fn delete_session(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unsaved = self.unsaved_in(|g| g.is(&conn, &info.id), cx);
        self.guard_unsaved(unsaved, window, cx, move |ws, window, cx| {
            ws.confirm_delete_session(conn, info, window, cx)
        });
    }

    fn confirm_delete_session(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        let title = info
            .title
            .clone()
            .unwrap_or_else(|| format!("Session {}", short_id(&info.id.0)));
        dialogs::confirm(
            "Delete this session?",
            format!(
                "\"{title}\" and its whole history will be removed from the agent. This cannot be undone."
            ),
            "Delete",
            move |window, cx| {
                let conn = conn.clone();
                let id = info.id.clone();
                this.update(cx, |ws, cx| {
                    ws.remove_groups(window, cx, |g| g.is(&conn, &id));
                    conn.update(cx, |conn, cx| conn.delete_session(id.clone(), cx));
                });
            },
            window,
            cx,
        );
    }

    fn archive_session(
        &mut self,
        conn: Entity<AgentConnection>,
        id: SessionId,
        archived: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unsaved = if archived {
            self.unsaved_in(|g| g.is(&conn, &id), cx)
        } else {
            Vec::new()
        };
        self.guard_unsaved(unsaved, window, cx, move |ws, window, cx| {
            if archived {
                ws.remove_groups(window, cx, |g| g.is(&conn, &id));
            }
            conn.update(cx, |conn, cx| conn.set_archived(id, archived, cx));
        });
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
        split: Option<SplitDirection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.active_group() else {
            notify::info(
                window,
                cx,
                "Open a session first; terminals belong to a session.",
            );
            return;
        };
        let conn = group.connection.clone();
        let session = group.session.clone();
        let origin = group.active_pane;
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
                    let remote = RemoteTerminal::new(&backend, client, id);
                    let rebind = remote.rebinder();
                    let io: Rc<dyn TerminalIo> = Rc::new(remote);
                    let font = mono_font(
                        appearance.mono_font.as_deref(),
                        &appearance.mono_font_fallbacks,
                        cx,
                    );
                    let size = px(appearance.mono_font_size);
                    let view = cx.new(|cx| TerminalView::new(Some(io), font, size, cx));
                    conn.update(cx, |conn, _| conn.register_terminal(id, &view, rebind));
                    this.subscriptions
                        .push(cx.observe(&view, |_, _, cx| cx.notify()));
                    let panel_id = this.alloc_panel();
                    let new_pane = this.alloc_panel();
                    let Some(gix) = this.groups.iter().position(|g| g.is(&conn, &session)) else {
                        return;
                    };
                    let group = &mut this.groups[gix];
                    let origin = if group.pane(origin).is_some() {
                        origin
                    } else {
                        group.active_pane
                    };
                    let target = match split {
                        Some(direction) => {
                            group.add_pane(origin, new_pane, direction);
                            new_pane
                        }
                        None => origin,
                    };
                    if let Some(pane) = group.pane_mut(target) {
                        pane.tabs.add(panel_id, Panel::Terminal(view), None);
                    }
                    group.active_pane = target;
                    this.activate_group(gix, window, cx);
                }
                Some(Err(err)) => notify::error(window, cx, err.to_string()),
                None => {}
            });
        })
        .detach();
    }

    fn stop_agent(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unsaved = self.unsaved_in(|g| g.connection == conn, cx);
        self.guard_unsaved(unsaved, window, cx, move |ws, window, cx| {
            ws.stop_agent_now(conn, window, cx)
        });
    }

    fn stop_agent_now(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = conn.read(cx);
        let client = read.client.clone();
        let instance = read.info.instance.clone();
        let host = read.host.name().to_owned();
        self.stopped.insert((host.clone(), instance.0.clone()));
        let task = read
            .backend
            .run(async move { client.stop_agent(&instance).await });
        self.drop_connection(&conn, window, cx);
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

    /// Closes a connection and its tabs.
    fn drop_connection(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_groups(window, cx, |g| g.connection == *conn);
        self.connections.retain(|c| c != conn);
        if self.active_connection.as_ref() == Some(conn) {
            self.active_connection = self.connections.first().cloned();
        }
    }

    /// Removes a disconnected agent from the panel, with its tabs. It comes
    /// back if its host lists it again.
    fn remove_agent(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unsaved = self.unsaved_in(|g| g.connection == conn, cx);
        self.guard_unsaved(unsaved, window, cx, move |ws, window, cx| {
            ws.remove_agent_now(conn, window, cx)
        });
    }

    fn remove_agent_now(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = conn.read(cx);
        let (host, instance) = (read.host.name().to_owned(), read.info.instance.0.clone());
        self.drop_connection(&conn, window, cx);
        self.forget_agent(&host, &instance);
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

    fn on_toggle_agents(&mut self, _: &ToggleAgents, _: &mut Window, cx: &mut Context<Self>) {
        if self.show_right && self.right_tab == RightTab::Agents {
            self.show_right = false;
        } else {
            self.show_right = true;
            self.right_tab = RightTab::Agents;
        }
        cx.notify();
    }

    fn on_toggle_right(&mut self, _: &ToggleRightDock, _: &mut Window, cx: &mut Context<Self>) {
        self.show_right = !self.show_right;
        cx.notify();
    }

    fn session_changes(&mut self, cx: &App) -> Option<Rc<Vec<FileChange>>> {
        let chat = self.active_group().and_then(SessionGroup::chat)?;
        let conversation = chat.read(cx).conversation.clone();
        let revision = conversation.read(cx).changes_revision;
        if let Some((id, cached, changes)) = &self.changes_cache
            && *id == conversation.entity_id()
            && *cached == revision
        {
            return Some(changes.clone());
        }
        let changes = Rc::new(session_changes(&conversation.read(cx).items));
        self.changes_cache = Some((conversation.entity_id(), revision, changes.clone()));
        Some(changes)
    }

    fn open_diff(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.active_group() else {
            return;
        };
        if let Some((pane, ix)) = group.find_diff(&path, false, cx) {
            self.activate_tab(pane, ix, window, cx);
            return;
        }
        let Some(chat) = group.chat() else {
            return;
        };
        let conversation = chat.read(cx).conversation.clone();
        let workspace = group.connection.read(cx).info.workspace.clone();
        let title = display_path(&path, &workspace);
        let wrap = self.config.appearance.wrap_lines;
        let view = cx.new(|cx| DiffView::session(conversation, path, title, wrap, cx));
        self.add_diff_tab(view, window, cx);
    }

    fn open_git_diff(
        &mut self,
        conn: Entity<AgentConnection>,
        change: GitChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = conn.read(cx).info.workspace.join(&change.path);
        if self.active_group().is_none() {
            notify::info(window, cx, "Open a session to view diffs.");
            return;
        }
        if let Some(group) = self.active_group()
            && let Some((pane, ix)) = group.find_diff(&path, true, cx)
        {
            self.activate_tab(pane, ix, window, cx);
            if let Some(Panel::Diff(view)) = self.active_group().and_then(|g| g.active_panel()) {
                view.update(cx, |view, cx| view.reload(cx));
            }
            return;
        }
        let wrap = self.config.appearance.wrap_lines;
        let view = cx.new(|cx| DiffView::git(conn, change.path, change.old_path, wrap, cx));
        self.add_diff_tab(view, window, cx);
    }

    fn add_diff_tab(
        &mut self,
        view: Entity<DiffView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        let id = self.alloc_panel();
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut()) {
            pane.tabs.add(id, Panel::Diff(view), None);
        }
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    /// The right panel: tabs for the agents list, the workspace's changes, and
    /// its files. Changes and files need a connection; agents do not.
    fn render_right_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let conn = self.changes_connection();
        let (listing, error) = conn
            .as_ref()
            .map(|c| {
                let git = &c.read(cx).git;
                (git.changes.clone(), git.error.clone())
            })
            .unwrap_or_default();
        if listing.is_none()
            && error.is_none()
            && let Some(conn) = conn.clone()
        {
            cx.defer(move |cx| conn.update(cx, |conn, cx| conn.refresh_changes(cx)));
        }
        let session = self.session_changes(cx);
        let (count, branch, body) = match (&conn, &listing) {
            (Some(conn), Some(listing)) if listing.repository => {
                let body = self.render_git_changes(conn.clone(), listing.clone(), session, cx);
                (listing.files.len(), listing.branch.clone(), body)
            }
            (_, None) if error.is_none() => (
                0,
                None,
                div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Loading changes…")
                    .into_any_element(),
            ),
            _ => {
                let note = match &error {
                    Some(error) => format!(
                        "Could not list git changes ({error}). Showing this session's changes."
                    ),
                    None => "Not a git repository. Showing this session's changes.".into(),
                };
                let count = session.as_ref().map(|c| c.len()).unwrap_or(0);
                (count, None, self.render_session_changes(session, note, cx))
            }
        };
        let tab = self.right_tab;
        let files_tab = tab == RightTab::Files;
        let changes_label = if count > 0 {
            format!("Changes {count}")
        } else {
            "Changes".to_string()
        };
        let tree = match (&conn, files_tab) {
            (Some(conn), true) => Some(self.file_tree(conn, window, cx)),
            _ => None,
        };
        if let Some(tree) = &tree {
            let selected = self
                .active_group()
                .and_then(|g| g.active_panel())
                .and_then(|panel| match panel {
                    Panel::File(view) => Some(view.read(cx).relative().to_string()),
                    _ => None,
                });
            if selected.is_some() {
                tree.update(cx, |tree, cx| tree.select(selected, cx));
            }
        }
        let body = match (tab, &tree) {
            (RightTab::Agents, _) => self.render_agents_panel(cx),
            (_, Some(tree)) => div()
                .flex_1()
                .min_h_0()
                .child(tree.clone())
                .into_any_element(),
            _ if conn.is_none() => div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Connect to an agent in the Agents tab to see its changes and files.")
                .into_any_element(),
            _ => body,
        };
        let header = h_flex()
            .w_full()
            .h(Tab::container_height())
            .flex_none()
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("right-agents")
                    .ghost()
                    .small()
                    .label("Agents")
                    .selected(tab == RightTab::Agents)
                    .tooltip("Agents and their sessions (ctrl-b)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Agents;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("right-changes")
                    .ghost()
                    .small()
                    .label(changes_label)
                    .selected(tab == RightTab::Changes)
                    .tooltip("Uncommitted changes (ctrl-shift-g)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Changes;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("right-files")
                    .ghost()
                    .small()
                    .label("Files")
                    .selected(files_tab)
                    .tooltip("Workspace files (ctrl-shift-e)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Files;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .when_some(branch.filter(|_| tab == RightTab::Changes), |el, branch| {
                el.child(
                    h_flex()
                        .min_w_0()
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(
                            Icon::new(gpui_kit_assets::IconName::GitBranch)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(branch),
                        ),
                )
            })
            .when_some(tree.clone(), |el, tree| {
                let collapse = tree.clone();
                el.child(
                    Button::new("refresh-files")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .tooltip("Refresh")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            tree.update(cx, |tree, cx| tree.refresh(cx))
                        })),
                )
                .child(
                    Button::new("collapse-files")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronsUpDown)
                        .tooltip("Collapse all folders")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            collapse.update(cx, |tree, cx| tree.collapse_all(cx))
                        })),
                )
            })
            .when(tab == RightTab::Agents, |el| {
                el.child(
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
                        .on_click(cx.listener(|this, _, window, cx| this.pair_dialog(window, cx))),
                )
            })
            .child(
                Button::new("collapse-right")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelRightClose)
                    .tooltip("Hide the right panel (ctrl-alt-b)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_right = false;
                        cx.notify();
                    })),
            );
        v_flex()
            .size_full()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_git_changes(
        &mut self,
        conn: Entity<AgentConnection>,
        listing: Rc<WorkspaceChanges>,
        session: Option<Rc<Vec<FileChange>>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        if listing.files.is_empty() {
            return div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No uncommitted changes.")
                .into_any_element();
        }
        let workspace = conn.read(cx).info.workspace.clone();
        let open = self.open_diff_path(true, cx);
        let touched: HashSet<PathBuf> = session
            .map(|changes| changes.iter().map(|c| c.path.clone()).collect())
            .unwrap_or_default();
        v_flex()
            .id("changes-list")
            .flex_1()
            .overflow_y_scroll()
            .py_1()
            .children(listing.files.iter().enumerate().map(|(ix, change)| {
                let absolute = workspace.join(&change.path);
                let (icon, color) = match change.status {
                    ChangeStatus::Added | ChangeStatus::Untracked => {
                        (gpui_kit_assets::IconName::FilePlus, theme.green)
                    }
                    ChangeStatus::Deleted => (gpui_kit_assets::IconName::FileMinus, theme.red),
                    ChangeStatus::Renamed | ChangeStatus::Copied => {
                        (gpui_kit_assets::IconName::FileSymlink, theme.blue)
                    }
                    ChangeStatus::Conflicted => {
                        (gpui_kit_assets::IconName::TriangleAlert, theme.yellow)
                    }
                    ChangeStatus::Modified | ChangeStatus::TypeChanged => {
                        (gpui_kit_assets::IconName::FilePen, theme.muted_foreground)
                    }
                };
                let detail = change.old_path.as_ref().map(|old| format!("← {old}"));
                let row = ChangeRow {
                    ix,
                    path: &change.path,
                    detail,
                    icon,
                    color,
                    added: change.added as usize,
                    removed: change.removed as usize,
                    binary: change.binary,
                    selected: open.as_ref() == Some(&absolute),
                    agent: touched.contains(&absolute),
                };
                let conn = conn.clone();
                let change = change.clone();
                row.render(&theme)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_git_diff(conn.clone(), change.clone(), window, cx)
                    }))
            }))
            .when(listing.truncated, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Showing the first {} files.", listing.files.len())),
                )
            })
            .into_any_element()
    }

    fn open_diff_path(&self, git: bool, cx: &App) -> Option<PathBuf> {
        self.active_group()
            .and_then(|g| g.active_panel())
            .and_then(|panel| match panel {
                Panel::Diff(view) if view.read(cx).is_git() == git => {
                    Some(view.read(cx).path().clone())
                }
                _ => None,
            })
    }

    fn render_session_changes(
        &mut self,
        changes: Option<Rc<Vec<FileChange>>>,
        note: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let workspace = self
            .changes_connection()
            .map(|c| c.read(cx).info.workspace.clone())
            .unwrap_or_default();
        let open = self.open_diff_path(false, cx);
        let message = |text: &str| {
            div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(text.to_owned())
        };
        let list = match changes {
            None => message("Open a session to see the files it changed.").into_any_element(),
            Some(changes) if changes.is_empty() => {
                message("No files changed in this session yet.").into_any_element()
            }
            Some(changes) => v_flex()
                .id("changes-list")
                .flex_1()
                .overflow_y_scroll()
                .py_1()
                .children(changes.iter().enumerate().map(|(ix, change)| {
                    let display = display_path(&change.path, &workspace);
                    let (icon, color) = if change.created {
                        (gpui_kit_assets::IconName::FilePlus, theme.green)
                    } else {
                        (gpui_kit_assets::IconName::FilePen, theme.muted_foreground)
                    };
                    let row = ChangeRow {
                        ix,
                        path: &display,
                        detail: None,
                        icon,
                        color,
                        added: change.added,
                        removed: change.removed,
                        binary: false,
                        selected: open.as_ref() == Some(&change.path),
                        agent: false,
                    };
                    let path = change.path.clone();
                    row.render(&theme)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_diff(path.clone(), window, cx)
                        }))
                }))
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(note),
            )
            .child(list)
            .into_any_element()
    }

    fn on_new_session(&mut self, _: &NewSession, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_connection.clone() {
            Some(conn) => self.new_session(conn, window, cx),
            None => notify::info(window, cx, "Connect to an agent first."),
        }
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(None, window, cx);
    }

    fn on_split_left(&mut self, _: &SplitLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.split_active(SplitDirection::Left, window, cx);
    }

    fn on_split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split_active(SplitDirection::Right, window, cx);
    }

    fn on_split_up(&mut self, _: &SplitUp, window: &mut Window, cx: &mut Context<Self>) {
        self.split_active(SplitDirection::Up, window, cx);
    }

    fn on_split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split_active(SplitDirection::Down, window, cx);
    }

    fn on_pane_left(&mut self, _: &ActivatePaneLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_pane_in_direction(SplitDirection::Left, window, cx);
    }

    fn on_pane_right(
        &mut self,
        _: &ActivatePaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_pane_in_direction(SplitDirection::Right, window, cx);
    }

    fn on_pane_up(&mut self, _: &ActivatePaneUp, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_pane_in_direction(SplitDirection::Up, window, cx);
    }

    fn on_pane_down(&mut self, _: &ActivatePaneDown, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_pane_in_direction(SplitDirection::Down, window, cx);
    }

    fn on_archive_session(
        &mut self,
        _: &ArchiveSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(group) = self.active_group() {
            let (conn, id) = (group.connection.clone(), group.session.clone());
            self.archive_session(conn, id, true, window, cx);
        }
    }

    fn on_rename_session(
        &mut self,
        _: &RenameSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.active_group().and_then(SessionGroup::chat) else {
            notify::info(window, cx, "Open a session to rename it.");
            return;
        };
        let chat = chat.read(cx);
        let conn = chat.connection.clone();
        let conversation = chat.conversation.read(cx);
        let (id, title) = (conversation.info.id.clone(), conversation.title());
        crate::ui::session::open_rename(conn, id, title, window, cx);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .active_group()
            .and_then(|g| g.active())
            .map(|p| (p.id, p.tabs.active_index()));
        if let Some((pane, ix)) = target {
            self.close_tab(pane, ix, window, cx);
        }
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut()) {
            pane.tabs.activate_next();
            self.focus_active_panel(window, cx);
            cx.notify();
        }
    }

    fn on_previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut()) {
            pane.tabs.activate_previous();
            self.focus_active_panel(window, cx);
            cx.notify();
        }
    }

    fn on_providers(&mut self, _: &ShowProviders, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(conn) = self.active_connection.clone() {
            dialogs::open_providers(conn, window, cx);
        }
    }

    fn on_choose_model(&mut self, _: &ChooseModel, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group() {
            let (conn, id) = (group.connection.clone(), group.session.clone());
            dialogs::open_model_picker(conn, Some(id), window, cx);
        }
    }

    fn binding_hint(&self, action: &dyn Action, window: &Window, cx: &App) -> Option<String> {
        let binding = window
            .bindings_for_action_in(action, &self.focus)
            .into_iter()
            .next_back()?;
        Some(
            binding
                .keystrokes()
                .iter()
                .map(|k| k.display(cx.platform_info().os).to_string())
                .collect::<Vec<_>>()
                .join(" "),
        )
    }

    fn on_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = window.focused(cx).unwrap_or_else(|| self.focus.clone());
        let items = palette_commands()
            .into_iter()
            .map(|(label, action)| {
                let hint = self.binding_hint(action.as_ref(), window, cx);
                let previous = previous.clone();
                PickerItem::new(label, move |window, cx| {
                    previous.focus(window, cx);
                    window.dispatch_action(action.boxed_clone(), cx);
                })
                .hint(hint)
            })
            .collect();
        open_picker("Command palette", "Run a command…", items, window, cx);
    }

    fn on_find_session(&mut self, _: &FindSession, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let mut items = Vec::new();
        let mut archived = Vec::new();
        for conn in &self.connections {
            let agent = conn.read(cx).title();
            for info in &conn.read(cx).sessions {
                let title = info
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Session {}", short_id(&info.id.0)));
                let (conn, info2, workspace) = (conn.clone(), info.clone(), workspace.clone());
                let item = PickerItem::new(title, move |window, cx| {
                    let (conn, info) = (conn.clone(), info2.clone());
                    let _ = workspace.update(cx, |ws, cx| ws.open_session(conn, info, window, cx));
                });
                if info.archived {
                    archived.push(item.detail(format!("{agent} · archived")));
                } else {
                    items.push(item.detail(agent.clone()));
                }
            }
        }
        items.extend(archived);
        open_picker("Sessions", "Find a session…", items, window, cx);
    }

    fn on_find_agent(&mut self, _: &FindAgent, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let mut items = Vec::new();
        for (hix, host) in self.hosts.iter().enumerate() {
            for agent in &host.agents {
                let conn = self.find_connection(host.entry.name(), &agent.instance, cx);
                let label = agent
                    .workspace
                    .as_ref()
                    .map(|w| w.display().to_string())
                    .unwrap_or_else(|| agent.instance.clone());
                let status = match conn.as_ref().map(|c| c.read(cx).status.clone()) {
                    Some(LinkStatus::Connected) => "connected",
                    Some(LinkStatus::Disconnected(_)) => "disconnected",
                    None => "not connected",
                };
                let instance = agent.instance.clone();
                let workspace = workspace.clone();
                items.push(
                    PickerItem::new(label, move |window, cx| {
                        let (conn, instance) = (conn.clone(), instance.clone());
                        let _ = workspace.update(cx, |ws, cx| match conn {
                            Some(conn) => {
                                if matches!(conn.read(cx).status, LinkStatus::Disconnected(_)) {
                                    ws.reconnect(&conn, window, cx);
                                } else {
                                    ws.active_connection = Some(conn);
                                    cx.notify();
                                }
                            }
                            None => ws.connect(hix, Target::Instance(instance), window, cx),
                        });
                    })
                    .detail(format!("{} · {status}", host.entry.name())),
                );
            }
        }
        open_picker("Agents", "Find an agent…", items, window, cx);
    }

    fn on_select_theme(&mut self, _: &SelectTheme, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let current = cx.theme().theme_name().clone();
        let items = crate::themes::names(cx)
            .into_iter()
            .map(|(name, mode)| {
                let workspace = workspace.clone();
                let label = name.clone();
                let detail = match (mode.is_dark(), name == current) {
                    (true, true) => "dark · current",
                    (true, false) => "dark",
                    (false, true) => "light · current",
                    (false, false) => "light",
                };
                PickerItem::new(label, move |window, cx| {
                    let name = name.clone();
                    let _ = workspace.update(cx, |ws, cx| {
                        ws.set_theme(name.to_string(), mode, window, cx)
                    });
                })
                .detail(detail)
            })
            .collect();
        open_picker("Themes", "Select a theme…", items, window, cx);
    }

    fn set_theme(
        &mut self,
        name: String,
        mode: gpui_component::ThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let appearance = &mut self.config.appearance;
        appearance.theme = if mode.is_dark() {
            loom_config::Theme::Dark
        } else {
            loom_config::Theme::Light
        };
        appearance.theme_name = Some(name);
        crate::themes::apply(&self.config.appearance, cx);
        self.save_config(window, cx);
        window.refresh();
        cx.notify();
    }

    fn on_add_ssh(&mut self, _: &AddSshHost, window: &mut Window, cx: &mut Context<Self>) {
        self.add_ssh_dialog(window, cx);
    }

    fn on_pair(&mut self, _: &PairRelay, window: &mut Window, cx: &mut Context<Self>) {
        self.pair_dialog(window, cx);
    }

    /// The agents the panel lists: every agent the hosts report, plus
    /// connections to agents they no longer list.
    fn panel_agents(&self, cx: &App) -> Vec<PanelAgent> {
        let home = std::env::var("HOME").ok();
        let mut agents = Vec::new();
        let mut seen = HashSet::new();
        let link_of = |conn: Option<&Entity<AgentConnection>>, key: &(String, String)| match conn
            .map(|c| c.read(cx).status.clone())
        {
            Some(LinkStatus::Connected) => Link::Connected,
            _ if self.connecting.contains(key) => Link::Connecting,
            Some(LinkStatus::Disconnected(reason)) => Link::Offline(Some(reason)),
            None => Link::Offline(self.connect_errors.get(key).cloned()),
        };
        let workspace_label = |host: &HostEntry, path: String| match host {
            HostEntry::Local { .. } => agent_list::short_path(&path, home.as_deref()),
            _ => path,
        };
        for (hix, host) in self.hosts.iter().enumerate() {
            for agent in &host.agents {
                let key = (host.entry.name().to_owned(), agent.instance.clone());
                if self.stopped.contains(&key) {
                    continue;
                }
                let conn = self.find_connection(host.entry.name(), &agent.instance, cx);
                if let Some(conn) = &conn {
                    seen.insert(conn.entity_id());
                }
                let path = agent
                    .workspace
                    .as_ref()
                    .map(|w| w.display().to_string())
                    .unwrap_or_else(|| format!("agent {}", short_id(&agent.instance)));
                agents.push(PanelAgent {
                    entry: AgentEntry {
                        remote: host.entry.name().to_owned(),
                        workspace: workspace_label(&host.entry, path),
                        link: link_of(conn.as_ref(), &key),
                        sessions: conn
                            .as_ref()
                            .map(|c| c.read(cx).sessions.clone())
                            .unwrap_or_default(),
                    },
                    host_ix: Some(hix),
                    instance: agent.instance.clone(),
                    conn,
                });
            }
        }
        for conn in &self.connections {
            if seen.contains(&conn.entity_id()) {
                continue;
            }
            let read = conn.read(cx);
            let key = (read.host.name().to_owned(), read.info.instance.0.clone());
            agents.push(PanelAgent {
                entry: AgentEntry {
                    remote: read.host.name().to_owned(),
                    workspace: workspace_label(
                        &read.host,
                        read.info.workspace.display().to_string(),
                    ),
                    link: link_of(Some(conn), &key),
                    sessions: read.sessions.clone(),
                },
                host_ix: self.hosts.iter().position(|h| h.entry == read.host),
                instance: read.info.instance.0.clone(),
                conn: Some(conn.clone()),
            });
        }
        agents
    }

    fn render_agents_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let agents = self.panel_agents(cx);
        let entries: Vec<AgentEntry> = agents.iter().map(|a| a.entry.clone()).collect();
        let groups = agent_list::group(&entries);
        let mut list = v_flex().w_full().gap_0p5().p_2();
        if groups.is_empty() {
            list = list.child(
                div()
                    .px_1()
                    .py_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child("No running agents. Start one for a workspace with + under Remotes."),
            );
        }
        for (group, rows) in groups {
            let collapsed = group == Group::Archived && !self.show_archived;
            list = list.child(self.render_group_header(group, rows.len(), collapsed, cx));
            if collapsed {
                continue;
            }
            for (ix, row) in rows.into_iter().enumerate() {
                list = list.child(self.render_agent_row(
                    group,
                    ix,
                    &agents[row.agent],
                    row.session,
                    cx,
                ));
            }
        }
        list = list.child(self.render_remotes(cx));
        div()
            .id("agents-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn render_group_header(
        &self,
        group: Group,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let color = match group {
            Group::NeedsApproval => theme.warning,
            Group::Working => theme.info,
            _ => theme.muted_foreground,
        };
        h_flex()
            .id(SharedString::from(format!("group-{group:?}")))
            .w_full()
            .gap_1()
            .px_1()
            .pt_2()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(color)
            .when(group == Group::Archived, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_archived = !this.show_archived;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall(),
                    )
            })
            .child(group.title())
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .font_weight(FontWeight::NORMAL)
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    /// One agent session, or an agent without one: its state, the session
    /// name, and where it runs.
    fn render_agent_row(
        &self,
        group: Group,
        ix: usize,
        agent: &PanelAgent,
        session: Option<SessionInfo>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let conn = agent.conn.clone();
        let unlinked = conn.is_none() && matches!(agent.entry.link, Link::Offline(_));
        let activity = session.as_ref().and_then(|s| {
            conn.as_ref()?
                .read(cx)
                .open_conversation(&s.id)?
                .read(cx)
                .activity()
        });
        let dot = |color| {
            div()
                .size(px(7.))
                .rounded_full()
                .bg(color)
                .into_any_element()
        };
        let spinner = |color| {
            Spinner::new()
                .icon(IconName::LoaderCircle)
                .xsmall()
                .color(color)
                .into_any_element()
        };
        let link = &agent.entry.link;
        let (indicator, title, detail): (AnyElement, String, Option<(String, gpui::Hsla)>) =
            match &session {
                Some(info) => {
                    let title = info
                        .title
                        .clone()
                        .unwrap_or_else(|| format!("Session {}", short_id(&info.id.0)));
                    let label = activity
                        .as_ref()
                        .map(Activity::label)
                        .unwrap_or_else(|| state_label(info.state).to_owned());
                    match group {
                        Group::NeedsApproval => (
                            Icon::new(IconName::TriangleAlert)
                                .xsmall()
                                .text_color(theme.warning)
                                .into_any_element(),
                            title,
                            Some((label, theme.warning)),
                        ),
                        Group::Working => (spinner(theme.info), title, Some((label, theme.info))),
                        Group::Archived => (
                            Icon::new(gpui_kit_assets::IconName::Archive)
                                .xsmall()
                                .text_color(theme.muted_foreground)
                                .into_any_element(),
                            title,
                            None,
                        ),
                        _ => (dot(theme.success), title, None),
                    }
                }
                None => match link {
                    Link::Connected => (dot(theme.muted_foreground), "No sessions".into(), None),
                    Link::Connecting => {
                        (spinner(theme.muted_foreground), "Connecting…".into(), None)
                    }
                    Link::Offline(reason) => (
                        dot(if reason.is_some() {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        }),
                        if agent.conn.is_some() {
                            "Disconnected".into()
                        } else {
                            "Not connected".into()
                        },
                        reason.clone().map(|r| (r, theme.danger)),
                    ),
                },
            };
        let placeholder = session.is_none();
        let active = session.as_ref().is_some_and(|s| {
            conn.as_ref()
                .is_some_and(|c| self.active_group().is_some_and(|g| g.is(c, &s.id)))
        });
        let place = format!("{} · {}", agent.entry.remote, agent.entry.workspace);
        let tooltip = SharedString::from(match &session {
            Some(info) => format!("{place}\n{}", state_label(info.state)),
            None => place.clone(),
        });
        let click_conn = conn.clone();
        let click_session = session.clone();
        let host_ix = agent.host_ix;
        let instance = agent.instance.clone();
        let connecting = *link == Link::Connecting;
        h_flex()
            .id(SharedString::from(format!("agent-row-{group:?}-{ix}")))
            .w_full()
            .items_start()
            .gap_2()
            .px_1()
            .py_1()
            .rounded_md()
            .when(active, |el| el.bg(theme.list_active))
            .hover(|s| s.bg(theme.list_hover))
            .when(!connecting, |el| el.cursor_pointer())
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                match (&click_conn, &click_session) {
                    (Some(conn), Some(info)) if event.click_count() >= 2 => {
                        crate::ui::session::open_rename(
                            conn.clone(),
                            info.id.clone(),
                            info.title.clone().unwrap_or_default(),
                            window,
                            cx,
                        )
                    }
                    (Some(conn), Some(info)) => {
                        this.open_session(conn.clone(), info.clone(), window, cx)
                    }
                    (Some(conn), None) if conn.read(cx).status == LinkStatus::Connected => {
                        this.new_session(conn.clone(), window, cx)
                    }
                    (Some(conn), None) => this.reconnect(conn, window, cx),
                    (None, _) if !connecting => {
                        if let Some(hix) = host_ix {
                            this.connect(hix, Target::Instance(instance.clone()), window, cx);
                        }
                    }
                    _ => {}
                }
            }))
            .child(div().h(px(20.)).flex().items_center().child(indicator))
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
                            .when(placeholder || group == Group::Archived, |el| {
                                el.text_color(theme.muted_foreground)
                            })
                            .child(title),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(place),
                    )
                    .when_some(detail, |el, (text, color)| {
                        el.child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_xs()
                                .text_color(color)
                                .child(text),
                        )
                    }),
            )
            .when_some(conn, |el, conn| {
                el.child(self.render_row_menu(group, ix, conn, session, cx))
            })
            .when(unlinked, |el| {
                el.child(self.render_unlinked_menu(group, ix, agent, cx))
            })
            .into_any_element()
    }

    /// The menu of an agent the host lists but the client has no connection
    /// to, such as one that failed to connect or has exited.
    fn render_unlinked_menu(
        &self,
        group: Group,
        ix: usize,
        agent: &PanelAgent,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let host_ix = agent.host_ix;
        let host = agent.entry.remote.clone();
        let instance = agent.instance.clone();
        div()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("agent-menu-{group:?}-{ix}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .dropdown_menu(move |mut menu, _, _| {
                        let item = |label: &'static str, run: Rc<MenuAction>| {
                            let workspace = workspace.clone();
                            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                let run = run.clone();
                                let _ = workspace.update(cx, |ws, cx| run(ws, window, cx));
                            })
                        };
                        if let Some(hix) = host_ix {
                            let id = instance.clone();
                            menu = menu.item(item(
                                "Connect",
                                Rc::new(move |ws, window, cx| {
                                    ws.connect(hix, Target::Instance(id.clone()), window, cx)
                                }),
                            ));
                            menu = menu.item(item(
                                "Look for agents again",
                                Rc::new(move |ws, window, cx| ws.refresh_host(hix, window, cx)),
                            ));
                        }
                        let (host, id) = (host.clone(), instance.clone());
                        menu.separator().item(item(
                            "Remove from list",
                            Rc::new(move |ws, _, cx| {
                                ws.forget_agent(&host, &id);
                                cx.notify();
                            }),
                        ))
                    }),
            )
            .into_any_element()
    }

    fn render_row_menu(
        &self,
        group: Group,
        ix: usize,
        conn: Entity<AgentConnection>,
        session: Option<SessionInfo>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let connected = conn.read(cx).status == LinkStatus::Connected;
        div()
            // The menu is inside the row, which must not see its clicks.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(SharedString::from(format!("agent-menu-{group:?}-{ix}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Ellipsis)
                    .dropdown_menu(move |mut menu, _, _| {
                        let item = |label: &'static str, run: Rc<MenuAction>| {
                            let workspace = workspace.clone();
                            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                let run = run.clone();
                                let _ = workspace.update(cx, |ws, cx| run(ws, window, cx));
                            })
                        };
                        if !connected {
                            let c = conn.clone();
                            menu = menu.item(item(
                                "Reconnect",
                                Rc::new(move |ws, window, cx| ws.reconnect(&c, window, cx)),
                            ));
                            let c = conn.clone();
                            return menu.separator().item(item(
                                "Remove from list",
                                Rc::new(move |ws, window, cx| {
                                    ws.remove_agent(c.clone(), window, cx)
                                }),
                            ));
                        }
                        let c = conn.clone();
                        menu = menu.item(item(
                            "New session in this workspace",
                            Rc::new(move |ws, window, cx| ws.new_session(c.clone(), window, cx)),
                        ));
                        if let Some(info) = &session {
                            let (c, i) = (conn.clone(), info.clone());
                            menu = menu.item(item(
                                "Rename…",
                                Rc::new(move |_, window, cx| {
                                    crate::ui::session::open_rename(
                                        c.clone(),
                                        i.id.clone(),
                                        i.title.clone().unwrap_or_default(),
                                        window,
                                        cx,
                                    )
                                }),
                            ));
                            let (c, id, archived) = (conn.clone(), info.id.clone(), info.archived);
                            menu = menu.item(item(
                                if archived { "Restore" } else { "Archive" },
                                Rc::new(move |ws, window, cx| {
                                    ws.archive_session(c.clone(), id.clone(), !archived, window, cx)
                                }),
                            ));
                            let (c, i) = (conn.clone(), info.clone());
                            menu = menu.item(item(
                                "Delete…",
                                Rc::new(move |ws, window, cx| {
                                    ws.delete_session(c.clone(), i.clone(), window, cx)
                                }),
                            ));
                        }
                        let c = conn.clone();
                        menu = menu.separator().item(item(
                            "Providers and sign-in…",
                            Rc::new(move |_, window, cx| {
                                dialogs::open_providers(c.clone(), window, cx)
                            }),
                        ));
                        let c = conn.clone();
                        menu.item(item(
                            "Stop agent",
                            Rc::new(move |ws, window, cx| ws.stop_agent(c.clone(), window, cx)),
                        ))
                    }),
            )
            .into_any_element()
    }

    /// The configured hosts, with their discovery errors and actions.
    fn render_remotes(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut section = v_flex().w_full().gap_0p5().child(
            div()
                .px_1()
                .pt_3()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child("Remotes"),
        );
        for (hix, host) in self.hosts.iter().enumerate() {
            let kind = match &host.entry {
                HostEntry::Local { .. } => "local".to_owned(),
                HostEntry::Ssh { destination, .. } => format!("ssh {destination}"),
                HostEntry::Relay { .. } => "relay".to_owned(),
            };
            let count = host.agents.len();
            section = section.child(
                v_flex()
                    .w_full()
                    .px_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(Icon::new(IconName::HardDrive).xsmall())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_sm()
                                    .child(host.entry.name().to_owned())
                                    .child(
                                        div()
                                            .ml_1()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!(
                                                "{kind} · {count} agent{}",
                                                if count == 1 { "" } else { "s" }
                                            )),
                                    )
                                    .flex()
                                    .items_baseline(),
                            )
                            .child(
                                Button::new(("refresh", hix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::RefreshCw)
                                    .loading(host.loading)
                                    .tooltip("Look for agents again")
                                    .on_click(cx.listener(move |this, _, window, cx| {
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
                                            this.remove_host(hix, window, cx)
                                        })),
                                )
                            }),
                    )
                    .when_some(host.error.clone(), |el, error| {
                        el.child(
                            div()
                                .pl_5()
                                .text_xs()
                                .text_color(theme.danger)
                                .whitespace_normal()
                                .child(error),
                        )
                    }),
            );
        }
        section.into_any_element()
    }

    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(group) = self.active_group() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(div().text_xl().child("Loom"))
                .child("Pick an agent in the Agents tab on the right, then open or create a session.")
                .child(
                    div().text_sm().child(
                        "ctrl-shift-p commands · ctrl-p sessions · ctrl-n new session · ctrl-` terminal · ctrl-b agents",
                    ),
                )
                .into_any_element();
        };
        let root = group.layout.root.clone();
        let active = group.active_pane;
        let multiple = group.panes.len() > 1;
        self.render_member(&root, active, multiple, cx)
    }

    fn render_member(
        &mut self,
        member: &Member,
        active: PaneId,
        multiple: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match member {
            Member::Pane(pane) => self.render_pane(*pane, *pane == active, multiple, cx),
            Member::Axis(axis) => {
                let id =
                    SharedString::from(format!("pane-axis-{}-{}", axis.id, axis.members.len()));
                let children: Vec<AnyElement> = axis
                    .members
                    .iter()
                    .map(|member| self.render_member(member, active, multiple, cx))
                    .collect();
                let group = match axis.axis {
                    Axis::Horizontal => h_resizable(id),
                    Axis::Vertical => v_resizable(id),
                };
                group
                    .children(
                        children
                            .into_iter()
                            .map(|child| resizable_panel().child(child)),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_pane(
        &mut self,
        pane_id: PaneId,
        is_active: bool,
        multiple: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(pane) = self.active_group().and_then(|g| g.pane(pane_id)) else {
            return div().into_any_element();
        };
        let len = pane.tabs.len();
        let active = pane.tabs.active_index();
        let panels: Vec<(PanelId, PanelKind, String, bool)> = pane
            .tabs
            .iter()
            .map(|(id, panel)| {
                let unsaved = panel.unsaved(cx).is_some();
                (id, panel.kind(), panel.title(cx), unsaved)
            })
            .collect();
        let content = pane.tabs.active().cloned();
        let dim = multiple && !is_active;
        let tabs = panels
            .into_iter()
            .enumerate()
            .map(|(ix, (id, kind, title, unsaved))| {
                let dragged = DraggedTab {
                    pane: pane_id,
                    panel: id,
                    ix,
                    kind,
                    title: title.clone().into(),
                };
                Tab::new(("tab", id as usize))
                    .position(Tab::position_for(ix, len, active))
                    .selected(ix == active)
                    .start_slot(panel_icon(kind))
                    .child(
                        div()
                            .max_w(px(240.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .when(dim, |el| el.text_color(theme.muted_foreground))
                            .child(title),
                    )
                    .when(unsaved, |tab| {
                        tab.child(
                            div()
                                .debug_selector(move || format!("tab-unsaved-{id}"))
                                .text_color(theme.muted_foreground)
                                .child("●"),
                        )
                    })
                    .end_slot(
                        Button::new(("close-tab", id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(pane_id, ix, window, cx)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_tab(pane_id, ix, window, cx)
                    }))
                    .on_middle_click(cx.listener(move |this, _, window, cx| {
                        this.close_tab(pane_id, ix, window, cx)
                    }))
                    .on_drag(dragged, |tab, _, _, cx| cx.new(|_| tab.clone()))
                    .drag_over::<DraggedTab>(move |style, dragged: &DraggedTab, _, cx| {
                        let theme = cx.theme();
                        let style = style
                            .bg(theme.drop_target)
                            .border_color(theme.drag_border)
                            .border_0();
                        if dragged.pane != pane_id || ix < dragged.ix {
                            style.border_l_2()
                        } else if ix > dragged.ix {
                            style.border_r_2()
                        } else {
                            style
                        }
                    })
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                        this.handle_tab_drop(dragged, pane_id, ix, window, cx)
                    }))
            });
        let tab_bar = h_flex()
            .w_full()
            .flex_none()
            .h(Tab::container_height())
            .bg(theme.tab_bar)
            .child(
                h_flex()
                    .id(("tabs-scroll", pane_id as usize))
                    .flex_shrink(1.)
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .child(
                div()
                    .id(("tab-bar-drop-target", pane_id as usize))
                    .flex_1()
                    .min_w_6()
                    .h_full()
                    .border_b_1()
                    .border_color(theme.border)
                    .child("")
                    .drag_over::<DraggedTab>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                        this.handle_tab_drop(dragged, pane_id, len, window, cx)
                    })),
            )
            .child(
                h_flex()
                    .h_full()
                    .px_1()
                    .gap_0p5()
                    .border_b_1()
                    .border_l_1()
                    .border_color(theme.border)
                    .child(
                        Button::new(("new-terminal", pane_id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::SquareTerminal)
                            .tooltip("New terminal in this pane (ctrl-`)")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_active_pane(pane_id, cx);
                                this.new_terminal(None, window, cx)
                            })),
                    )
                    .child(
                        Button::new(("split-right", pane_id as usize))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(gpui_kit_assets::IconName::Columns2))
                            .tooltip("Split right (ctrl-k right)")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_active_pane(pane_id, cx);
                                this.split_active(SplitDirection::Right, window, cx)
                            })),
                    )
                    .child(
                        Button::new(("split-down", pane_id as usize))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(gpui_kit_assets::IconName::Rows2))
                            .tooltip("Split down (ctrl-k down)")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_active_pane(pane_id, cx);
                                this.split_active(SplitDirection::Down, window, cx)
                            })),
                    ),
            );
        let inner = match content {
            Some(Panel::Terminal(view)) => div().size_full().p_1().child(view).into_any_element(),
            Some(panel) => div().size_full().child(panel.view()).into_any_element(),
            None => div().size_full().into_any_element(),
        };
        let group_name = SharedString::from(format!("pane-{pane_id}"));
        let split = match self.drag_split {
            Some((target, direction)) if target == pane_id => direction,
            _ => None,
        };
        let half = DefiniteLength::Fraction(0.5);
        let overlay = div()
            .id(("pane-drop", pane_id as usize))
            .invisible()
            .absolute()
            .bg(theme.drop_target)
            .group_drag_over::<DraggedTab>(group_name.clone(), |style| style.visible())
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                this.handle_split_drop(dragged, pane_id, window, cx)
            }))
            .map(|el| match split {
                None => el.top_0().right_0().bottom_0().left_0(),
                Some(SplitDirection::Up) => el.top_0().left_0().right_0().h(half),
                Some(SplitDirection::Down) => el.left_0().bottom_0().right_0().h(half),
                Some(SplitDirection::Left) => el.top_0().left_0().bottom_0().w(half),
                Some(SplitDirection::Right) => el.top_0().bottom_0().right_0().w(half),
            });
        let body = div()
            .id(("pane-body", pane_id as usize))
            .group(group_name)
            .relative()
            .flex_1()
            .w_full()
            .overflow_hidden()
            .on_drag_move::<DraggedTab>(cx.listener(
                move |this, event: &DragMoveEvent<DraggedTab>, _, cx| {
                    this.handle_drag_move(pane_id, event, cx)
                },
            ))
            .child(inner)
            .child(overlay);
        v_flex()
            .id(("pane", pane_id as usize))
            .size_full()
            .capture_any_mouse_down(
                cx.listener(move |this, _, _, cx| this.set_active_pane(pane_id, cx)),
            )
            .child(tab_bar)
            .child(body)
            .into_any_element()
    }

    fn render_status(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut left = h_flex().gap_3();
        let mut right = h_flex().gap_3();
        if let Some(conn) = &self.active_connection {
            let conn = conn.read(cx);
            let (transport, transport_icon) = match &conn.host {
                HostEntry::Local { .. } => ("local socket", IconName::HardDrive),
                HostEntry::Ssh { .. } => ("ssh", IconName::Network),
                HostEntry::Relay { .. } => ("relay (end-to-end encrypted)", IconName::Globe),
            };
            let agent_tooltip: SharedString =
                format!("Agent {} over {transport}", conn.title()).into();
            left = left.child(
                h_flex()
                    .id("status-agent")
                    .gap_1()
                    .child(Icon::new(transport_icon).xsmall())
                    .child(conn.title())
                    .tooltip(move |window, cx| {
                        Tooltip::new(agent_tooltip.clone()).build(window, cx)
                    }),
            );
            let (status_icon, status, status_tooltip, color) = match &conn.status {
                LinkStatus::Connected => (
                    Icon::new(gpui_kit_assets::IconName::Plug),
                    "connected".to_owned(),
                    format!("Connected over {transport}"),
                    theme.muted_foreground,
                ),
                LinkStatus::Disconnected(reason) => (
                    Icon::new(gpui_kit_assets::IconName::Unplug),
                    format!("disconnected: {reason}"),
                    format!("Disconnected from the agent ({transport}): {reason}"),
                    theme.danger,
                ),
            };
            left = left.child(
                h_flex()
                    .id("status-link")
                    .gap_1()
                    .text_color(color)
                    .child(status_icon.xsmall())
                    .child(status)
                    .tooltip(move |window, cx| {
                        Tooltip::new(status_tooltip.clone()).build(window, cx)
                    }),
            );
            if conn.status == LinkStatus::Connected
                && let Some(item) = telemetry_item(conn, &theme, cx)
            {
                left = left.child(item);
            }
        } else {
            left = left.child(
                h_flex()
                    .gap_1()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(gpui_kit_assets::IconName::Unplug).xsmall())
                    .child("not connected"),
            );
        }
        if let Some(tab) = self.active_group().and_then(SessionGroup::chat) {
            right = right.children(SessionView::status_items(&tab, cx));
        }
        let show_right = self.show_right;
        right = right.child(
            Button::new("toggle-right")
                .ghost()
                .xsmall()
                .icon(IconName::PanelRight)
                .selected(show_right)
                .tooltip("Toggle the right panel (ctrl-alt-b)")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_right = !this.show_right;
                    cx.notify();
                })),
        );
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

/// The status bar item of the agent's OpenTelemetry export; clicking it
/// asks again. Nothing until the first answer arrives.
fn telemetry_item(
    conn: &AgentConnection,
    theme: &gpui_component::Theme,
    cx: &Context<Workspace>,
) -> Option<AnyElement> {
    let summary = match (&conn.telemetry.status, &conn.telemetry.error) {
        (Some(status), _) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            crate::telemetry::summarize(status, now)
        }
        (None, Some(error)) => crate::telemetry::unknown(error),
        (None, None) => return None,
    };
    let color = match summary.health {
        Health::Off | Health::Waiting => theme.muted_foreground,
        Health::Healthy => theme.success,
        Health::Failing => theme.danger,
        Health::Unknown => theme.warning,
    };
    let tooltip = SharedString::from(format!("{}\nClick to check again.", summary.tooltip));
    Some(
        h_flex()
            .id("status-otel")
            .gap_1()
            .cursor_pointer()
            .text_color(color)
            .child(Icon::new(gpui_kit_assets::IconName::Activity).xsmall())
            .child(summary.label)
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(conn) = this.active_connection.clone() {
                    conn.update(cx, |conn, cx| conn.refresh_telemetry(true, cx));
                }
            }))
            .into_any_element(),
    )
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
        let rem = window.rem_size();
        let name_lift = text_lift(window, &theme.font_family, FontWeight::SEMIBOLD, rem);
        let subtitle_lift = text_lift(window, &theme.font_family, FontWeight::NORMAL, rem * 0.875);
        let center = self.render_center(cx);
        let right = self.show_right.then(|| self.render_right_panel(window, cx));
        let status = self.render_status(cx);
        v_flex()
            .id("workspace")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::on_toggle_agents))
            .on_action(cx.listener(Self::on_toggle_right))
            .on_action(cx.listener(Self::on_command_palette))
            .on_action(cx.listener(Self::on_find_session))
            .on_action(cx.listener(Self::on_find_agent))
            .on_action(cx.listener(Self::on_select_theme))
            .on_action(cx.listener(Self::on_new_session))
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_archive_session))
            .on_action(cx.listener(Self::on_rename_session))
            .on_action(cx.listener(Self::on_show_changes))
            .on_action(cx.listener(Self::on_show_files))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(Self::on_providers))
            .on_action(cx.listener(Self::on_choose_model))
            .on_action(cx.listener(Self::on_add_ssh))
            .on_action(cx.listener(Self::on_pair))
            .on_action(cx.listener(Self::on_split_left))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_split_up))
            .on_action(cx.listener(Self::on_split_down))
            .on_action(cx.listener(Self::on_pane_left))
            .on_action(cx.listener(Self::on_pane_right))
            .on_action(cx.listener(Self::on_pane_up))
            .on_action(cx.listener(Self::on_pane_down))
            .child(
                TitleBar::new()
                    .on_close_window(
                        cx.listener(|this, _, window, cx| this.close_window(window, cx)),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                img(crate::assets::logo())
                                    .flex_none()
                                    .size(px(18.))
                                    .debug_selector(|| "titlebar-logo".into()),
                            )
                            .child(
                                div()
                                    .relative()
                                    .top(-name_lift)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .debug_selector(|| "titlebar-name".into())
                                    .child("Loom"),
                            )
                            .when_some(self.active_connection.as_ref(), |el, conn| {
                                el.child(
                                    div()
                                        .relative()
                                        .top(-subtitle_lift)
                                        .debug_selector(|| "titlebar-subtitle".into())
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(conn.read(cx).title()),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(match right {
                        Some(right) => h_resizable("center-right")
                            .child(resizable_panel().child(center))
                            .child(
                                resizable_panel()
                                    .size(px(300.))
                                    .size_range(px(200.)..px(600.))
                                    .child(right),
                            )
                            .into_any_element(),
                        None => center,
                    }),
            )
            .child(status)
    }
}

struct ChangeRow<'a> {
    ix: usize,
    path: &'a str,
    detail: Option<String>,
    icon: gpui_kit_assets::IconName,
    color: gpui::Hsla,
    added: usize,
    removed: usize,
    binary: bool,
    selected: bool,
    agent: bool,
}

impl ChangeRow<'_> {
    fn render(self, theme: &gpui_component::Theme) -> gpui::Stateful<gpui::Div> {
        let (dir, name) = match self.path.rsplit_once('/') {
            Some((dir, name)) => (dir.to_owned(), name.to_owned()),
            None => (String::new(), self.path.to_owned()),
        };
        let secondary = self.detail.unwrap_or(dir);
        h_flex()
            .id(("change", self.ix))
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .cursor_pointer()
            .when(self.selected, |el| el.bg(theme.list_active))
            .hover(|el| el.bg(theme.list_hover))
            .child(Icon::new(self.icon).xsmall().text_color(self.color))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().flex_none().child(name))
                    .child(
                        div()
                            .text_xs()
                            .text_ellipsis()
                            .overflow_hidden()
                            .text_color(theme.muted_foreground)
                            .child(secondary),
                    ),
            )
            .when(self.agent, |el| {
                el.child(
                    Icon::new(gpui_kit_assets::IconName::Bot)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
            })
            .when(self.binary, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("binary"),
                )
            })
            .when(!self.binary, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.green)
                        .child(format!("+{}", self.added)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.red)
                        .child(format!("−{}", self.removed)),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{Action, TestAppContext, VisualTestContext};
    use gpui_component::{Root, WindowExt};
    use loom_config::Dirs;

    use super::*;
    use crate::test_agent::{TestAgent, wait_until};

    /// A workspace connected to `agent`, with a session open and `path`
    /// open in a file tab, and the file's editor focused.
    fn open_file<'a>(
        agent: &TestAgent,
        path: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let conn = agent.connect(cx);
        let root = agent.dir.path().to_path_buf();
        let env = Env {
            dirs: Dirs {
                home: root.clone(),
                config_home: root.join("gui-config"),
                data_home: root.join("gui-data"),
                state_home: root.join("gui-state"),
                runtime_dir: None,
            },
            agent_bin: "/nonexistent/loom-agent".into(),
        };
        let backend = agent.backend.clone();
        let slot = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let slot = slot.clone();
            move |window, cx| {
                let config_path = root.join("client.yaml");
                let ws = cx.new(|cx| {
                    Workspace::new(
                        backend,
                        env,
                        ClientConfig::default(),
                        config_path,
                        window,
                        cx,
                    )
                });
                *slot.borrow_mut() = Some(ws.clone());
                Root::new(ws, window, cx)
            }
        });
        let ws: Entity<Workspace> = slot.borrow_mut().take().unwrap();
        ws.update_in(cx, |ws, window, cx| {
            ws.connections.push(conn.clone());
            ws.watch_connection(&conn, window, cx);
            ws.active_connection = Some(conn.clone());
            ws.new_session(conn.clone(), window, cx);
        });
        wait_until(cx, |cx| ws.read(cx).active_group.is_some());
        ws.update_in(cx, |ws, window, cx| {
            ws.open_file(conn.clone(), path.into(), window, cx)
        });
        wait_until(cx, |cx| {
            file_view(&ws, cx).is_some_and(|view| view.read(cx).is_editable())
        });
        ws.update_in(cx, |ws, window, cx| ws.focus_active_panel(window, cx));
        cx.run_until_parked();
        (ws, cx)
    }

    fn file_view(ws: &Entity<Workspace>, cx: &App) -> Option<Entity<FileView>> {
        ws.read(cx)
            .groups
            .iter()
            .flat_map(|g| g.panes.iter())
            .flat_map(|p| p.tabs.iter())
            .find_map(|(_, panel)| match panel {
                Panel::File(view) => Some(view.clone()),
                _ => None,
            })
    }

    fn click(selector: &'static str, cx: &mut VisualTestContext) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("no {selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn has_dialog(cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| window.has_active_dialog(cx))
    }

    #[test]
    fn keymaps_naming_the_old_left_panel_action_still_work() {
        assert_eq!(ToggleAgents.name(), "loom::ToggleAgents");
        assert_eq!(ToggleAgents::deprecated_aliases(), ["loom::ToggleLeftDock"]);
    }

    #[gpui::test]
    fn the_title_bar_shows_the_logo_left_of_the_name(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (_, cx) = open_file(&agent, "a.txt", cx);
        // The test font draws its letters about 0.05em low. A large size makes
        // that several pixels, so the lift can't vanish into rounding.
        cx.update(|window, cx| {
            gpui_component::Theme::global_mut(cx).font_size = px(40.);
            window.refresh();
        });
        cx.run_until_parked();
        let logo = cx.debug_bounds("titlebar-logo").expect("no logo");
        let name = cx.debug_bounds("titlebar-name").expect("no name");
        let subtitle = cx.debug_bounds("titlebar-subtitle").expect("no subtitle");
        assert_eq!(logo.size, gpui::size(px(18.), px(18.)));
        assert!(logo.right() <= name.left(), "{logo:?} {name:?}");

        // Where the text looks centred: the middle of its capitals, a quarter
        // of the way toward that of its lowercase letters. gpui centres the
        // font's ascent and descent in the line.
        let text_middle = |cx: &mut VisualTestContext,
                           bounds: gpui::Bounds<Pixels>,
                           weight: FontWeight,
                           scale: f32| {
            cx.update(|window, cx| {
                let mut font = gpui::font(cx.theme().font_family.clone());
                font.weight = weight;
                let size = window.rem_size() * scale;
                let text = window.text_system();
                let id = text.resolve_font(&font);
                let ascent = text.ascent(id, size);
                let descent = text.descent(id, size).abs();
                let letters = (text.cap_height(id, size) * 3. + text.x_height(id, size)) / 8.;
                let baseline = bounds.top() + (bounds.size.height - ascent - descent) / 2. + ascent;
                baseline - letters
            })
        };
        // The lift is whole pixels, so the text may be half a pixel off.
        let logo_middle = logo.top() + logo.size.height / 2.;
        let name_middle = text_middle(cx, name, FontWeight::SEMIBOLD, 1.);
        let subtitle_middle = text_middle(cx, subtitle, FontWeight::NORMAL, 0.875);
        for middle in [name_middle, subtitle_middle] {
            assert!(
                (middle - logo_middle).abs() <= px(0.5),
                "text at {middle:?}, logo at {logo_middle:?}: {logo:?} {name:?} {subtitle:?}"
            );
        }
    }

    #[gpui::test]
    fn closing_a_tab_with_unsaved_changes_asks_first(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        let id = ws.read_with(cx, |ws, cx| {
            let pane = ws.active_group().unwrap().active().unwrap();
            assert!(pane.tabs.active().unwrap().unsaved(cx).is_none());
            pane.tabs.iter().nth(pane.tabs.active_index()).unwrap().0
        });
        let dot = format!("tab-unsaved-{id}");
        cx.simulate_input("x");
        assert!(cx.debug_bounds(dot.clone().leak()).is_some());

        cx.simulate_keystrokes("ctrl-w");
        assert!(has_dialog(cx));
        assert!(cx.update(|_, cx| file_view(&ws, cx)).is_some());
        click("unsaved-cancel", cx);
        assert!(!has_dialog(cx));
        assert!(cx.update(|_, cx| file_view(&ws, cx)).is_some());

        cx.simulate_keystrokes("ctrl-w");
        click("unsaved-discard", cx);
        assert!(!has_dialog(cx));
        assert!(cx.update(|_, cx| file_view(&ws, cx)).is_none());
        assert_eq!(agent.read("a.txt"), b"one\n");
    }

    #[gpui::test]
    fn the_close_prompt_saves_and_then_closes(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        cx.simulate_input("x");
        cx.simulate_keystrokes("ctrl-w");
        click("unsaved-save", cx);
        wait_until(cx, |cx| file_view(&ws, cx).is_none());
        assert_eq!(agent.read("a.txt"), b"xone\n");
    }

    #[gpui::test]
    fn a_failed_save_from_the_close_prompt_keeps_the_tab(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        cx.simulate_input("x");
        agent.write("a.txt", "theirs\n");
        cx.simulate_keystrokes("ctrl-w");
        click("unsaved-save", cx);
        let view = cx.update(|_, cx| file_view(&ws, cx)).unwrap();
        wait_until(cx, |cx| !view.read(cx).is_saving());
        assert!(cx.update(|_, cx| file_view(&ws, cx)).is_some());
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        assert_eq!(agent.read("a.txt"), b"theirs\n");
    }

    #[gpui::test]
    fn archiving_a_session_with_unsaved_changes_asks_first(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        cx.simulate_input("x");
        let (conn, session) = ws.read_with(cx, |ws, _| {
            let group = ws.active_group().unwrap();
            (group.connection.clone(), group.session.clone())
        });
        ws.update_in(cx, |ws, window, cx| {
            ws.archive_session(conn.clone(), session.clone(), true, window, cx)
        });
        assert!(has_dialog(cx));
        assert_eq!(ws.read_with(cx, |ws, _| ws.groups.len()), 1);
        click("unsaved-discard", cx);
        assert_eq!(ws.read_with(cx, |ws, _| ws.groups.len()), 0);
    }

    #[gpui::test]
    fn quitting_or_closing_the_window_asks_about_unsaved_changes(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (_ws, cx) = open_file(&agent, "a.txt", cx);
        assert!(cx.simulate_close(), "nothing unsaved, so the window closes");

        cx.simulate_input("x");
        assert!(!cx.simulate_close());
        assert!(has_dialog(cx));
        click("unsaved-cancel", cx);

        cx.simulate_keystrokes("ctrl-q");
        assert!(has_dialog(cx));
        click("unsaved-cancel", cx);
        assert!(!has_dialog(cx));
    }
}
