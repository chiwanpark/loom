mod codespaces;

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
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::tooltip::Tooltip;
use gpui_component::{
    ActiveTheme, Icon, IconName, Selectable, Sizable, TitleBar, h_flex, h_resizable,
    resizable_panel, v_flex, v_resizable,
};
use loom_client::proto::{
    ChangeStatus, FileChange as GitChange, Notice, SessionId, SessionInfo, SessionState,
    TerminalHolder, WorkspaceChanges,
};
use loom_config::{ClientConfig, HostEntry};
use loom_view_model::codespace_list::Activity;
use loom_view_model::host_colors;

use crate::backend::Backend;
use crate::changes::{FileChange, display_path, session_changes};
use crate::connection::{self, AgentSummary, Env};
use crate::conversation::short_id;
use crate::layout::{self, GroupKey, SavedGroup, SavedLayout, SavedPane, SavedTab};
use crate::oracle::OracleLink;
use crate::state::{AgentConnection, Attached, ConnectionEvent, LinkStatus, RemoteTerminal};
use crate::telemetry::Health;
use crate::term::{TakeOver, TerminalIo, TerminalView, mono_font};
use crate::ui::dialogs::{self, FormField, open_form};
use crate::ui::diff::{DiffKind, DiffView};
use crate::ui::file_view::FileView;
use crate::ui::files::{FileTreeEvent, FileTreeView};
use crate::ui::notes_view::NotesView;
use crate::ui::notify;
use crate::ui::panes::{Axis, Member, PaneGroup, PaneId, SplitDirection};
use crate::ui::picker::{PickerItem, open_picker};
use crate::ui::ports_view::PortsView;
use crate::ui::prompt_view::SystemPromptView;
use crate::ui::session::SessionView;
use crate::ui::settings::SettingsView;
use crate::ui::tab::Tab;
use crate::ui::tabs::{PanelId, TabList};
use crate::ui::tasks_view::TasksView;

actions!(
    loom,
    [
        /// Shows the Codespaces tab of the right panel, or hides the panel
        /// when it is already showing. The tab used to be the Sessions tab,
        /// and before that the Agents tab.
        #[action(deprecated_aliases = [
            "loom::ToggleSessions",
            "loom::ToggleLeftDock",
            "loom::ToggleAgents"
        ])]
        ToggleCodespaces,
        NewSession,
        NewTerminal,
        NewCodespace,
        OracleSettings,
        CloseTab,
        /// Archives the current session and closes its tab. It used to be
        /// CloseSession too.
        #[action(deprecated_aliases = ["loom::CloseSession"])]
        ArchiveSession,
        ResumeSession,
        NextTab,
        PreviousTab,
        /// Shows the first tab of the active pane.
        ActivateTab1,
        /// Shows the second tab of the active pane.
        ActivateTab2,
        /// Shows the third tab of the active pane.
        ActivateTab3,
        /// Shows the fourth tab of the active pane.
        ActivateTab4,
        /// Shows the fifth tab of the active pane.
        ActivateTab5,
        /// Shows the sixth tab of the active pane.
        ActivateTab6,
        /// Shows the seventh tab of the active pane.
        ActivateTab7,
        /// Shows the eighth tab of the active pane.
        ActivateTab8,
        /// Shows the last tab of the active pane.
        ActivateLastTab,
        /// Moves the active tab one place to the left in its pane.
        MoveTabLeft,
        /// Moves the active tab one place to the right in its pane.
        MoveTabRight,
        ShowProviders,
        ChooseModel,
        #[action(deprecated_aliases = ["loom::PairRelay"])]
        PairOracle,
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
        /// Shows the Codespaces tab of the right panel.
        #[action(deprecated_aliases = ["loom::ShowSessions", "loom::ShowAgents"])]
        ShowCodespaces,
        /// Shows the active agent's notes in the right panel.
        ShowNotes,
        /// Shows the active session's task list in the right panel.
        ShowTasks,
        /// Shows the active agent's listening and forwarded ports in the
        /// right panel.
        ShowPorts,
        /// Opens a tab with the active session's system prompt and tools.
        ShowSystemPrompt,
        /// Wraps long lines in the focused file or diff view, or stops
        /// wrapping them.
        ToggleLineWrap,
        /// Saves the focused file.
        Save,
        /// Looks for a newer release of Loom and offers to install it.
        CheckForUpdates
    ]
);

const CONTEXT: &str = "Workspace";
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const CHANGES_POLL: Duration = Duration::from_secs(2);
/// How long after the start the client first looks for a newer release.
const UPDATE_CHECK_DELAY: Duration = Duration::from_secs(10);
/// How often the client looks for a newer release after that.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const RECONNECT_ATTEMPTS: usize = 20;
const OUTSIDE_TERMINAL: &str = "Workspace && !Terminal";
const DROP_TARGET_SIZE: f32 = 0.2;

pub fn bind_keys(overrides: &BTreeMap<String, String>, cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-b", ToggleCodespaces, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-alt-b", ToggleRightDock, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-e", ShowFiles, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-g", ShowChanges, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-p", ToggleCommandPalette, Some(CONTEXT)),
        KeyBinding::new("ctrl-p", FindSession, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-alt-o", FindAgent, Some(CONTEXT)),
        KeyBinding::new("ctrl-k ctrl-t", SelectTheme, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-n", NewSession, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-alt-n", NewCodespace, Some(CONTEXT)),
        KeyBinding::new("ctrl-w", CloseTab, Some(OUTSIDE_TERMINAL)),
        KeyBinding::new("ctrl-`", NewTerminal, Some(CONTEXT)),
        KeyBinding::new("ctrl-pagedown", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-pageup", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, Some(CONTEXT)),
        KeyBinding::new("alt-1", ActivateTab1, Some(CONTEXT)),
        KeyBinding::new("alt-2", ActivateTab2, Some(CONTEXT)),
        KeyBinding::new("alt-3", ActivateTab3, Some(CONTEXT)),
        KeyBinding::new("alt-4", ActivateTab4, Some(CONTEXT)),
        KeyBinding::new("alt-5", ActivateTab5, Some(CONTEXT)),
        KeyBinding::new("alt-6", ActivateTab6, Some(CONTEXT)),
        KeyBinding::new("alt-7", ActivateTab7, Some(CONTEXT)),
        KeyBinding::new("alt-8", ActivateTab8, Some(CONTEXT)),
        KeyBinding::new("alt-9", ActivateLastTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-pageup", MoveTabLeft, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-pagedown", MoveTabRight, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-b", ShowCodespaces, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-n", ShowNotes, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-t", ShowTasks, Some(CONTEXT)),
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
        KeyBinding::new("ctrl-s", Save, Some(crate::ui::notes_view::CONTEXT)),
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

/// What to do with a connection once the client is connected.
pub(crate) type AfterConnect =
    Box<dyn FnOnce(&mut Workspace, Entity<AgentConnection>, &mut Window, &mut Context<Workspace>)>;

struct HostNode {
    entry: HostEntry,
    agents: Vec<AgentSummary>,
    loading: bool,
    error: Option<String>,
    oracle: Option<Entity<OracleLink>>,
}

#[derive(Clone)]
enum Panel {
    Chat(Entity<SessionView>),
    Terminal(Entity<TerminalView>),
    Diff(Entity<DiffView>),
    File(Entity<FileView>),
    SystemPrompt(Entity<SystemPromptView>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Chat,
    Terminal,
    Diff,
    File,
    SystemPrompt,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RightTab {
    Codespaces,
    Changes,
    Files,
    Notes,
    Tasks,
    Ports,
}

/// Something with changes that are not saved, which closing must not lose
/// without asking.
#[derive(Clone)]
enum Unsaved {
    File(Entity<FileView>),
    Notes(Entity<NotesView>),
}

impl Unsaved {
    /// What the prompt calls it: a file's path, or the notes.
    fn name(&self, cx: &App) -> String {
        match self {
            Unsaved::File(view) => view.read(cx).relative().to_string(),
            Unsaved::Notes(_) => "the notes".into(),
        }
    }

    fn save(&self, window: &mut Window, cx: &mut App) -> gpui::Task<bool> {
        match self {
            Unsaved::File(view) => view.update(cx, |v, cx| v.save(window, cx)),
            Unsaved::Notes(view) => view.update(cx, |v, cx| v.save(window, cx)),
        }
    }
}

impl Panel {
    fn kind(&self) -> PanelKind {
        match self {
            Panel::Chat(_) => PanelKind::Chat,
            Panel::Terminal(_) => PanelKind::Terminal,
            Panel::Diff(_) => PanelKind::Diff,
            Panel::File(_) => PanelKind::File,
            Panel::SystemPrompt(_) => PanelKind::SystemPrompt,
        }
    }

    fn view(&self) -> AnyView {
        match self {
            Panel::Chat(view) => view.clone().into(),
            Panel::Terminal(view) => view.clone().into(),
            Panel::Diff(view) => view.clone().into(),
            Panel::File(view) => view.clone().into(),
            Panel::SystemPrompt(view) => view.clone().into(),
        }
    }

    fn title(&self, cx: &App) -> String {
        match self {
            Panel::Chat(view) => view.read(cx).title(cx),
            Panel::Diff(view) => view.read(cx).title(),
            Panel::File(view) => view.read(cx).title(),
            Panel::SystemPrompt(_) => "System prompt".into(),
            Panel::Terminal(view) => {
                let view = view.read(cx);
                let base = view.title().unwrap_or_else(|| "Terminal".into());
                match view.holder() {
                    _ if view.exited().is_some() => format!("{base} (exited)"),
                    TerminalHolder::You => base,
                    TerminalHolder::Other(client) => {
                        format!("{base} (on {})", client.describe())
                    }
                    TerminalHolder::Nobody => format!("{base} (detached)"),
                }
            }
        }
    }

    /// Whether the tab can be closed. Closing a session's tab archives the
    /// session. A terminal that another client holds, or none does, stays
    /// until it is taken over here, since closing a terminal ends its
    /// shell.
    fn closable(&self, cx: &App) -> bool {
        match self {
            Panel::Chat(_) => true,
            Panel::Terminal(view) => {
                let view = view.read(cx);
                view.exited().is_some() || *view.holder() == TerminalHolder::You
            }
            Panel::Diff(_) | Panel::File(_) | Panel::SystemPrompt(_) => true,
        }
    }

    /// The file view, if this is one with unsaved changes.
    fn unsaved(&self, cx: &App) -> Option<Unsaved> {
        match self {
            Panel::File(view) if view.read(cx).is_dirty() => Some(Unsaved::File(view.clone())),
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
            Panel::SystemPrompt(view) => view.read(cx).editor_focus(cx).focus(window, cx),
        }
    }
}

/// The last part of a slash-separated path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The small round state mark of a row in the Sessions tab.
fn dot(color: gpui::Hsla) -> AnyElement {
    div()
        .size(px(7.))
        .rounded_full()
        .bg(color)
        .into_any_element()
}

/// The spinning state mark of a row in the Sessions tab.
fn spinner(color: gpui::Hsla) -> AnyElement {
    Spinner::new()
        .icon(IconName::LoaderCircle)
        .xsmall()
        .color(color)
        .into_any_element()
}

fn palette_commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("workspace: new session", Box::new(NewSession)),
        ("codespace: new codespace", Box::new(NewCodespace)),
        ("oracle: settings", Box::new(OracleSettings)),
        ("workspace: new terminal", Box::new(NewTerminal)),
        ("workspace: close tab", Box::new(CloseTab)),
        ("workspace: next tab", Box::new(NextTab)),
        ("workspace: previous tab", Box::new(PreviousTab)),
        ("workspace: first tab", Box::new(ActivateTab1)),
        ("workspace: last tab", Box::new(ActivateLastTab)),
        ("workspace: move tab left", Box::new(MoveTabLeft)),
        ("workspace: move tab right", Box::new(MoveTabRight)),
        ("workspace: toggle codespaces", Box::new(ToggleCodespaces)),
        ("workspace: toggle right panel", Box::new(ToggleRightDock)),
        ("panel: codespaces", Box::new(ShowCodespaces)),
        ("panel: changes", Box::new(ShowChanges)),
        ("panel: files", Box::new(ShowFiles)),
        ("panel: notes", Box::new(ShowNotes)),
        ("panel: tasks", Box::new(ShowTasks)),
        ("panel: ports", Box::new(ShowPorts)),
        ("view: toggle line wrap", Box::new(ToggleLineWrap)),
        ("file: save", Box::new(Save)),
        ("session: archive", Box::new(ArchiveSession)),
        ("session: resume", Box::new(ResumeSession)),
        ("session: rename", Box::new(RenameSession)),
        ("session: choose model", Box::new(ChooseModel)),
        ("session: show system prompt", Box::new(ShowSystemPrompt)),
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
        ("hosts: pair with an oracle", Box::new(PairOracle)),
        ("app: check for updates", Box::new(CheckForUpdates)),
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
        PanelKind::SystemPrompt => Icon::new(gpui_kit_assets::IconName::ScrollText).xsmall(),
    }
}

fn session_icon(state: SessionState, theme: &gpui_component::Theme) -> AnyElement {
    match Activity::of(state) {
        Activity::NeedsApproval => Icon::new(IconName::TriangleAlert)
            .xsmall()
            .text_color(theme.warning)
            .into_any_element(),
        Activity::Working => spinner(theme.info),
        Activity::Completed => Icon::new(IconName::CircleCheck)
            .xsmall()
            .text_color(theme.success)
            .into_any_element(),
        Activity::Idle => panel_icon(PanelKind::Chat).into_any_element(),
    }
}

fn session_tab_menu(
    menu: gpui_component::menu::PopupMenu,
    workspace: gpui::WeakEntity<Workspace>,
    conn: Entity<AgentConnection>,
    session: SessionId,
    title: String,
) -> gpui_component::menu::PopupMenu {
    let rename = (conn.clone(), session.clone());
    let prompt = (workspace.clone(), conn.clone(), session.clone());
    menu.item(
        PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
            let (conn, session) = rename.clone();
            crate::ui::session::open_rename(conn, session, title.clone(), window, cx)
        }),
    )
    .item(
        PopupMenuItem::new("System prompt").on_click(move |_, window, cx| {
            let (workspace, conn, session) = prompt.clone();
            let _ = workspace.update(cx, |ws, cx| {
                let info = conn
                    .read(cx)
                    .sessions
                    .iter()
                    .find(|s| s.id == session)
                    .cloned();
                if let Some(info) = info {
                    ws.open_session(conn.clone(), info, window, cx);
                    ws.show_system_prompt(window, cx);
                }
            });
        }),
    )
    .separator()
    .item(
        PopupMenuItem::new("Archive").on_click(move |_, window, cx| {
            let _ = workspace.update(cx, |ws, cx| {
                ws.archive_session(conn.clone(), session.clone(), window, cx)
            });
        }),
    )
}

struct Pane {
    id: PaneId,
    tabs: TabList<Panel>,
}

struct TabGroup {
    connection: Entity<AgentConnection>,
    session: Option<SessionId>,
    panes: Vec<Pane>,
    layout: PaneGroup,
    active_pane: PaneId,
    /// The current session's task list, once the Tasks tab has shown it.
    tasks: Option<Entity<TasksView>>,
}

impl TabGroup {
    fn new(connection: Entity<AgentConnection>, pane: PaneId) -> Self {
        Self {
            connection,
            session: None,
            panes: vec![Pane {
                id: pane,
                tabs: TabList::default(),
            }],
            layout: PaneGroup::new(pane),
            active_pane: pane,
            tasks: None,
        }
    }

    fn chat(&self, cx: &App) -> Option<Entity<SessionView>> {
        self.chat_of(self.session.as_ref()?, cx)
    }

    fn chats(&self) -> impl Iterator<Item = &Entity<SessionView>> {
        self.panes
            .iter()
            .flat_map(|pane| pane.tabs.iter())
            .filter_map(|(_, panel)| match panel {
                Panel::Chat(view) => Some(view),
                Panel::Terminal(_) | Panel::Diff(_) | Panel::File(_) | Panel::SystemPrompt(_) => {
                    None
                }
            })
    }

    fn chat_of(&self, session: &SessionId, cx: &App) -> Option<Entity<SessionView>> {
        self.chats()
            .find(|view| view.read(cx).session_id(cx) == *session)
            .cloned()
    }

    fn find_chat(&self, session: &SessionId, cx: &App) -> Option<(PaneId, usize)> {
        self.panes.iter().find_map(|pane| {
            pane.tabs
                .find(|panel| {
                    matches!(panel, Panel::Chat(view) if view.read(cx).session_id(cx) == *session)
                })
                .map(|ix| (pane.id, ix))
        })
    }

    fn find_diff(&self, path: &PathBuf, kind: DiffKind, cx: &App) -> Option<(PaneId, usize)> {
        self.panes.iter().find_map(|pane| {
            pane.tabs
                .find(|panel| {
                    matches!(panel, Panel::Diff(view)
                        if view.read(cx).path() == path && view.read(cx).kind() == kind)
                })
                .map(|ix| (pane.id, ix))
        })
    }

    fn is(&self, conn: &Entity<AgentConnection>) -> bool {
        &self.connection == conn
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
    active_connection: Option<Entity<AgentConnection>>,
    groups: Vec<TabGroup>,
    active_group: Option<usize>,
    /// Each agent's notes, by its connection's entity, once the Notes tab
    /// has shown them.
    notes: HashMap<EntityId, Entity<NotesView>>,
    /// Each agent's ports, by its connection's entity, once the Ports tab
    /// has shown them.
    ports: HashMap<EntityId, Entity<PortsView>>,
    settings: HashMap<String, Entity<SettingsView>>,
    shown_settings: Option<Entity<SettingsView>>,
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
                oracle: None,
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
            active_connection: None,
            groups: Vec::new(),
            active_group: None,
            drag_split: None,
            next_panel: 0,
            show_right: true,
            changes_cache: None,
            right_tab: RightTab::Codespaces,
            file_trees: Vec::new(),
            poll_ticks: 0,
            focus: cx.focus_handle(),
            subscriptions: Vec::new(),
            layout_path,
            pending_layout: saved_layout.groups.clone(),
            pending_active: saved_layout.active.clone(),
            saved_layout,
            restoring: HashMap::new(),
            notes: HashMap::new(),
            ports: HashMap::new(),
            settings: HashMap::new(),
            shown_settings: None,
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
                if ws.unsaved_everywhere(cx).is_empty() {
                    true
                } else {
                    ws.close_window(window, cx);
                    false
                }
            })
            .unwrap_or(true)
        });
        this.subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                this.see_active_session(window, cx);
            }));
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
        // Debug builds are for development, where a release would replace
        // the build.
        if this.config.check_for_updates && !cfg!(debug_assertions) {
            let handle = window.window_handle();
            cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
                let mut delay = UPDATE_CHECK_DELAY;
                loop {
                    cx.background_executor().timer(delay).await;
                    delay = UPDATE_CHECK_INTERVAL;
                    let checked = handle.update(cx, |_, window, cx| {
                        this.update(cx, |ws, cx| ws.check_for_updates(false, window, cx))
                    });
                    if !matches!(checked, Ok(Ok(()))) {
                        break;
                    }
                }
            })
            .detach();
        }
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
            notify::info(window, cx, "Open a codespace to view files.");
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

    fn on_show_notes(&mut self, _: &ShowNotes, window: &mut Window, cx: &mut Context<Self>) {
        self.show_right = true;
        self.right_tab = RightTab::Notes;
        if let Some(view) = self.notes_view(window, cx) {
            view.read(cx).editor_focus(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn on_show_tasks(&mut self, _: &ShowTasks, window: &mut Window, cx: &mut Context<Self>) {
        self.show_right = true;
        self.right_tab = RightTab::Tasks;
        if let Some(view) = self.tasks_view(window, cx) {
            view.read(cx).input_focus(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn on_show_ports(&mut self, _: &ShowPorts, window: &mut Window, cx: &mut Context<Self>) {
        self.show_right = true;
        self.right_tab = RightTab::Ports;
        self.ports_view(window, cx);
        cx.notify();
    }

    /// The ports view of the agent the Ports tab shows, the same as the
    /// Notes tab's, made the first time it is asked for.
    fn ports_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<PortsView>> {
        let conn = self.notes_connection()?;
        if let Some(view) = self.ports.get(&conn.entity_id()) {
            return Some(view.clone());
        }
        let id = conn.entity_id();
        let view = cx.new(|cx| PortsView::new(conn, window, cx));
        self.ports.insert(id, view.clone());
        Some(view)
    }

    /// The agent whose notes the Notes tab shows: the active session's, or
    /// the active agent's when no session is open.
    fn notes_connection(&self) -> Option<Entity<AgentConnection>> {
        match self.active_group() {
            Some(group) => Some(group.connection.clone()),
            None => self.active_connection.clone(),
        }
    }

    /// The notes view of the agent the Notes tab shows, made the first time
    /// it is asked for.
    fn notes_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<NotesView>> {
        let conn = self.notes_connection()?;
        if let Some(view) = self.notes.get(&conn.entity_id()) {
            return Some(view.clone());
        }
        let id = conn.entity_id();
        let view = cx.new(|cx| NotesView::new(conn, window, cx));
        self.notes.insert(id, view.clone());
        Some(view)
    }

    /// The active session's tasks view, made the first time it is asked for.
    fn tasks_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TasksView>> {
        let group = self.active_group_mut()?;
        let session = group.session.clone()?;
        if let Some(view) = &group.tasks
            && *view.read(cx).session() == session
        {
            return Some(view.clone());
        }
        let conn = group.connection.clone();
        let view = cx.new(|cx| TasksView::new(conn, session, window, cx));
        self.active_group_mut()?.tasks = Some(view.clone());
        Some(view)
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
        if let Some(chat) = self.active_group().and_then(|g| g.chat(cx))
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
        self.refresh_oracle(ix, window, cx);
        self.list_host(ix, true, window, cx);
    }

    /// Lists the host's codespace agents again. With `retry_failed`, agents
    /// whose connection failed are tried again; otherwise they wait for a
    /// click, so a failure that triggers a listing cannot loop.
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
        let name = host.entry.name().to_owned();
        if retry_failed {
            self.connect_errors.retain(|(host, _), _| *host != name);
        }
        self.sync_oracle(ix, window, cx);
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
        instance: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect_with(host_ix, instance, false, None, window, cx);
    }

    /// Connects to a codespace agent, and runs `then` with its connection
    /// once the client is connected.
    pub(crate) fn connect_then(
        &mut self,
        host_ix: usize,
        instance: String,
        then: AfterConnect,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect_with(host_ix, instance, false, Some(then), window, cx);
    }

    /// Connects to a codespace agent. A `background` connection neither
    /// reports errors nor replaces the active agent. `then` runs once the
    /// client is connected.
    fn connect_with(
        &mut self,
        host_ix: usize,
        instance: String,
        background: bool,
        then: Option<AfterConnect>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.hosts.get(host_ix) else {
            return;
        };
        let entry = host.entry.clone();
        let key = (entry.name().to_owned(), instance.clone());
        if !self.connecting.insert(key.clone()) {
            if then.is_some() {
                notify::info(
                    window,
                    cx,
                    "The client is connecting to this agent. Try again when it is connected.",
                );
            }
            return;
        }
        let env = self.env.clone();
        let backend = self.backend.clone();
        let task = {
            let entry = entry.clone();
            self.backend
                .run(async move { connection::connect(&env, &entry, &instance).await })
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
                            this.active_connection = Some(conn.clone());
                        }
                        if let Some(old) = &replaced {
                            this.forget_agent(entry.name(), old);
                        }
                        if (!background || replaced.is_some())
                            && let Some(ix) = this.hosts.iter().position(|h| h.entry == entry)
                        {
                            this.list_host(ix, !background, window, cx);
                        }
                        if let Some(then) = then {
                            then(this, conn, window, cx);
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
                        if let Some(ix) = this.hosts.iter().position(|h| h.entry == entry) {
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
                        let instance = conn.read(cx).info.instance.0.clone();
                        let host = conn.read(cx).host.name().to_owned();
                        if let Some(ix) = this.hosts.iter().position(|h| h.entry.name() == host) {
                            this.connect_with(ix, instance, true, None, window, cx);
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
            .push(cx.observe_in(conn, window, |this, conn, window, cx| {
                if this.active_group().is_some_and(|g| g.connection == conn) {
                    this.see_active_session(window, cx);
                }
                cx.notify()
            }));
        self.subscriptions.push(cx.subscribe_in(
            conn,
            window,
            |this, conn, event: &ConnectionEvent, window, cx| match event {
                ConnectionEvent::Notice(notice) => {
                    let (kind, message) = notice_toast(&conn.read(cx).title(), notice);
                    match kind {
                        Toast::Info => notify::info(window, cx, message),
                        Toast::Success => notify::success(window, cx, message),
                        Toast::Error => {
                            if let Notice::ConfigRejected { message } = notice {
                                tracing::warn!("{message}");
                            }
                            notify::error(window, cx, message)
                        }
                    }
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
                ConnectionEvent::SessionRemoved(_) | ConnectionEvent::SessionChanged(_) => {
                    this.sync_session_tabs(conn, window, cx);
                }
                ConnectionEvent::SessionsLoaded => {
                    this.restore_groups(conn, window, cx);
                    this.sync_session_tabs(conn, window, cx);
                    this.sync_terminal_tabs(conn, &[], window, cx);
                }
                ConnectionEvent::TerminalsChanged { closed } => {
                    this.sync_terminal_tabs(conn, closed, window, cx)
                }
                ConnectionEvent::LoginFinished(_)
                | ConnectionEvent::NotesChanged { .. }
                | ConnectionEvent::TasksChanged { .. } => {}
            },
        ));
    }

    fn reconnect(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let instance = conn.read(cx).info.instance.0.clone();
        let host = conn.read(cx).host.name().to_owned();
        if let Some(ix) = self.hosts.iter().position(|h| h.entry.name() == host) {
            self.connect(ix, instance, window, cx);
        }
    }

    /// How a tab is opened again after a restart, or `None` when it is not
    /// worth keeping (an ended terminal).
    fn saved_tab(&self, group: &TabGroup, panel: &Panel, cx: &App) -> Option<SavedTab> {
        match panel {
            Panel::Chat(view) => Some(SavedTab::Conversation {
                session: view.read(cx).session_id(cx).0,
            }),
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
            Panel::Diff(view) => Some(view.read(cx).saved_tab(cx)),
            Panel::File(view) => Some(SavedTab::File {
                path: view.read(cx).relative().to_owned(),
            }),
            Panel::SystemPrompt(view) => Some(SavedTab::SystemPrompt {
                session: view.read(cx).session().0.clone(),
            }),
        }
    }

    fn group_key(group: &TabGroup, cx: &App) -> GroupKey {
        let conn = group.connection.read(cx);
        GroupKey {
            host: conn.host.name().to_owned(),
            workspace: conn.info.workspace.clone(),
        }
    }

    fn saved_group(&self, group: &TabGroup, cx: &App) -> Option<SavedGroup> {
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
    /// listed its sessions. The tabs of sessions that are gone or archived
    /// are dropped.
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
            if self.groups.iter().any(|g| g.is(conn)) {
                continue;
            }
            if let Some(group) = self.restore_group(conn, &sessions, &saved, window, cx) {
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
        sessions: &[SessionInfo],
        saved: &SavedGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TabGroup> {
        let mut saved_panes = Vec::new();
        let root =
            layout::restore_member(&saved.root, &mut || self.alloc_panel(), &mut saved_panes)?;
        let mut panes = Vec::new();
        for (id, pane) in saved_panes {
            let mut tabs = TabList::default();
            for tab in &pane.tabs {
                if let Some(panel) = self.restore_panel(conn, sessions, tab, window, cx) {
                    let panel_id = self.alloc_panel();
                    tabs.add(panel_id, panel, Some(tabs.len()));
                }
            }
            tabs.activate(pane.active.min(tabs.len().saturating_sub(1)));
            panes.push(Pane { id, tabs });
        }
        let order: Vec<PaneId> = panes.iter().map(|p| p.id).collect();
        let mut group = TabGroup {
            connection: conn.clone(),
            session: None,
            layout: PaneGroup::from_root(root),
            active_pane: order.get(saved.active_pane).copied().unwrap_or(order[0]),
            panes,
            tasks: None,
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
        group.session = match group.active_panel() {
            Some(Panel::Chat(view)) => Some(view.read(cx).session_id(cx)),
            _ => group
                .chats()
                .next()
                .map(|view| view.read(cx).session_id(cx)),
        };
        (!group.is_empty()).then_some(group)
    }

    fn restore_panel(
        &mut self,
        conn: &Entity<AgentConnection>,
        sessions: &[SessionInfo],
        tab: &SavedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Panel> {
        let wrap = self.config.appearance.wrap_lines;
        let open = |session: &str| sessions.iter().find(|s| s.id.0 == session && !s.archived);
        let panel = match tab {
            SavedTab::Conversation { session } => {
                let info = open(session)?.clone();
                return Some(self.chat_panel(conn, &info, window, cx));
            }
            SavedTab::Terminal { instance, id } => {
                return Some(Panel::Terminal(
                    self.restore_terminal(conn, instance, *id, window, cx),
                ));
            }
            SavedTab::GitDiff {
                path,
                old_path,
                committed,
            } => {
                let view = cx.new(|cx| {
                    DiffView::git(
                        conn.clone(),
                        path.clone(),
                        old_path.clone(),
                        *committed,
                        wrap,
                        cx,
                    )
                });
                self.subscriptions
                    .push(cx.observe(&view, |_, _, cx| cx.notify()));
                Panel::Diff(view)
            }
            SavedTab::SessionDiff { path, session } => {
                let info = open(session)?.clone();
                let conversation = conn.update(cx, |conn, cx| conn.conversation(&info, cx));
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
            SavedTab::SystemPrompt { session } => {
                let id = open(session)?.id.clone();
                self.system_prompt_panel(conn, id, window, cx)
            }
        };
        Some(panel)
    }

    fn system_prompt_panel(
        &mut self,
        conn: &Entity<AgentConnection>,
        session: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Panel {
        let view = cx.new(|cx| SystemPromptView::new(conn.clone(), session, window, cx));
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        Panel::SystemPrompt(view)
    }

    /// A terminal view in the configured font, which offers to take its
    /// terminal over when another client holds it.
    fn terminal_view(
        &mut self,
        io: Option<Rc<dyn TerminalIo>>,
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
        let view = cx.new(|cx| TerminalView::new(io, font, size, cx));
        self.subscriptions
            .push(cx.observe(&view, |_, _, cx| cx.notify()));
        self.subscriptions.push(cx.subscribe_in(
            &view,
            window,
            |this, view, _: &TakeOver, _, cx| this.take_over_terminal(view, cx),
        ));
        view
    }

    /// A terminal tab that reattaches to its terminal when the same agent
    /// still runs it (unless another client holds it now), and opens a new
    /// one in its place otherwise.
    fn restore_terminal(
        &mut self,
        conn: &Entity<AgentConnection>,
        instance: &str,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        let view = self.terminal_view(None, window, cx);
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
        let owner = read.terminal_owner;
        let (cols, rows) = view.read(cx).grid_size();
        // Register before attaching, so the output that the redraw after
        // attaching produces reaches the view.
        let existing = (read.info.instance.0 == instance).then(|| {
            let remote = RemoteTerminal::new(&backend, client.clone(), id);
            let rebind = remote.rebinder();
            conn.update(cx, |conn, _| {
                conn.register_terminal(id, &view, rebind, true)
            });
            Rc::new(remote) as Rc<dyn TerminalIo>
        });
        let attach = existing
            .is_some()
            .then(|| conn.update(cx, |conn, cx| conn.attach_terminal(id, false, cx)));
        let conn = conn.clone();
        let view_handle = view.clone();
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let attached = match attach {
                Some(task) => task.await,
                None => Attached::Failed("the agent restarted".into()),
            };
            if let Attached::Failed(failed) = &attached {
                tracing::debug!(terminal = id, "cannot reattach: {failed}");
            } else {
                let _ = this.update_in(cx, |this, _, cx| {
                    this.restoring.remove(&view_handle.entity_id());
                    if let Some(io) = existing {
                        view_handle.update(cx, |view, cx| view.connect(io, cx));
                    }
                    cx.notify();
                });
                return;
            }
            if existing.is_some() {
                let _ = this.update(cx, |_, cx| {
                    conn.update(cx, |conn, _| conn.unregister_terminal(id))
                });
            }
            let result = backend
                .run({
                    let client = client.clone();
                    async move { client.open_terminal(cols, rows, owner).await }
                })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                this.restoring.remove(&view_handle.entity_id());
                let io = match result {
                    Some(Ok(new_id)) => {
                        let remote = RemoteTerminal::new(&backend, client, new_id);
                        let rebind = remote.rebinder();
                        conn.update(cx, |conn, _| {
                            conn.register_terminal(new_id, &view_handle, rebind, false)
                        });
                        Some(Rc::new(remote) as Rc<dyn TerminalIo>)
                    }
                    Some(Err(err)) => {
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

    fn open_group(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if let Some(ix) = self.groups.iter().position(|g| g.is(conn)) {
            return ix;
        }
        let pane = self.alloc_panel();
        self.groups.push(TabGroup::new(conn.clone(), pane));
        let ix = self.groups.len() - 1;
        self.sync_session_tabs(conn, window, cx);
        self.sync_terminal_tabs(conn, &[], window, cx);
        ix
    }

    pub(crate) fn show_group(
        &mut self,
        conn: Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.open_group(&conn, window, cx);
        self.activate_group(ix, window, cx);
    }

    pub(crate) fn open_session(
        &mut self,
        conn: Entity<AgentConnection>,
        info: SessionInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ix = self.open_group(&conn, window, cx);
        if self.groups[ix].find_chat(&info.id, cx).is_none() {
            let panel = self.chat_panel(&conn, &info, window, cx);
            let id = self.alloc_panel();
            if let Some(pane) = self.groups[ix].active_mut() {
                pane.tabs.add(id, panel, None);
            }
        }
        if let Some((pane, tab)) = self.groups[ix].find_chat(&info.id, cx) {
            let group = &mut self.groups[ix];
            group.active_pane = pane;
            if let Some(pane) = group.pane_mut(pane) {
                pane.tabs.activate(tab);
            }
            group.session = Some(info.id.clone());
        }
        self.activate_group(ix, window, cx);
    }

    fn sync_session_tabs(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.groups.iter().any(|g| g.is(conn)) {
            return;
        }
        let sessions: Vec<SessionInfo> = conn.read(cx).sessions.clone();
        let open: HashSet<&SessionId> = sessions
            .iter()
            .filter(|s| !s.archived)
            .map(|s| &s.id)
            .collect();
        let closed: Vec<PanelId> = self
            .groups
            .iter()
            .filter(|g| g.is(conn))
            .flat_map(|g| g.panes.iter())
            .flat_map(|pane| pane.tabs.iter())
            .filter(|(_, panel)| match panel {
                Panel::Chat(view) => !open.contains(&view.read(cx).session_id(cx)),
                _ => false,
            })
            .map(|(id, _)| id)
            .collect();
        let mut changed = !closed.is_empty();
        for id in closed {
            self.close_panel(id, window, cx);
        }
        for info in sessions.iter().rev().filter(|s| !s.archived) {
            let Some(gix) = self.groups.iter().position(|g| g.is(conn)) else {
                return;
            };
            if self.groups[gix].find_chat(&info.id, cx).is_some() {
                continue;
            }
            let panel = self.chat_panel(conn, info, window, cx);
            let id = self.alloc_panel();
            let group = &mut self.groups[gix];
            let target = group
                .panes
                .iter()
                .position(|pane| pane.tabs.find(|p| matches!(p, Panel::Chat(_))).is_some())
                .or_else(|| group.panes.iter().position(|p| p.id == group.active_pane))
                .unwrap_or(0);
            group.panes[target].tabs.push_background(id, panel);
            changed = true;
        }
        if changed {
            self.update_current_sessions(cx);
            self.see_active_session(window, cx);
            cx.notify();
        }
    }

    fn active_group(&self) -> Option<&TabGroup> {
        self.active_group.and_then(|ix| self.groups.get(ix))
    }

    fn activate_group(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.groups.get(ix) else {
            return;
        };
        self.active_group = Some(ix);
        self.shown_settings = None;
        self.active_connection = Some(group.connection.clone());
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    /// Tells the agent that the user sees the session of the active tab,
    /// while the window is active, so that a completed session becomes idle.
    fn see_active_session(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            return;
        }
        if let Some(group) = self.active_group()
            && let Some(Panel::Chat(view)) = group.active_panel()
        {
            let session = view.read(cx).session_id(cx);
            group
                .connection
                .update(cx, |conn, cx| conn.mark_seen(&session, cx));
        }
    }

    fn update_current_sessions(&mut self, cx: &App) {
        for group in &mut self.groups {
            let active = match group.active_panel() {
                Some(Panel::Chat(view)) => Some(view.read(cx).session_id(cx)),
                _ => None,
            };
            let current = active.or_else(|| {
                group
                    .session
                    .clone()
                    .filter(|id| group.find_chat(id, cx).is_some())
            });
            group.session = current.or_else(|| {
                group
                    .chats()
                    .next()
                    .map(|view| view.read(cx).session_id(cx))
            });
        }
    }

    fn focus_active_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.active_group().and_then(|g| g.active_panel()).cloned() {
            panel.focus(window, cx);
        }
        self.update_current_sessions(cx);
        self.see_active_session(window, cx);
    }

    fn active_group_mut(&mut self) -> Option<&mut TabGroup> {
        self.active_group.and_then(|g| self.groups.get_mut(g))
    }

    fn set_active_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if let Some(group) = self.active_group_mut()
            && group.active_pane != pane
            && group.pane(pane).is_some()
        {
            group.active_pane = pane;
            self.update_current_sessions(cx);
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

    fn release_panel(group: &TabGroup, panel: &Panel, cx: &mut Context<Self>) {
        match panel {
            Panel::Chat(view) => {
                let id = view.read(cx).session_id(cx);
                group
                    .connection
                    .update(cx, |conn, _| conn.release_conversation(&id));
            }
            // Another client's terminal goes on there; it gets a tab in
            // another tab group of the agent.
            Panel::Terminal(view) if *view.read(cx).holder() != TerminalHolder::You => {
                group.connection.update(cx, |conn, _| {
                    if let Some(id) = conn.terminal_id(view) {
                        conn.unregister_terminal(id);
                    }
                });
            }
            _ => {}
        }
    }

    /// Closes a tab of the active group that can be closed, after asking
    /// what to do with its unsaved changes, if it has any. Closing a
    /// session's tab archives the session.
    fn close_tab(&mut self, pane: PaneId, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.active_group() else {
            return;
        };
        let Some((id, panel)) = group.pane(pane).and_then(|p| p.tabs.iter().nth(ix)) else {
            return;
        };
        if !panel.closable(cx) {
            return;
        }
        if let Panel::Chat(view) = panel {
            let session = view.read(cx).session_id(cx);
            let conn = group.connection.clone();
            self.archive_session(conn, session, window, cx);
            return;
        }
        let unsaved: Vec<Unsaved> = panel.unsaved(cx).into_iter().collect();
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
        let group = &self.groups[gix];
        Self::release_panel(group, &panel, cx);
        self.focus_active_panel(window, cx);
        cx.notify();
    }

    /// File views with unsaved changes in the groups that match.
    fn unsaved_in(&self, pred: impl Fn(&TabGroup) -> bool, cx: &App) -> Vec<Unsaved> {
        self.groups
            .iter()
            .filter(|g| pred(g))
            .flat_map(|g| {
                g.panes
                    .iter()
                    .flat_map(|p| p.tabs.iter())
                    .filter_map(|(_, panel)| panel.unsaved(cx))
            })
            .collect()
    }

    /// The notes view of `conn`, or of every agent, if it has unsaved
    /// changes.
    fn unsaved_notes(&self, conn: Option<&Entity<AgentConnection>>, cx: &App) -> Vec<Unsaved> {
        self.notes
            .iter()
            .filter(|(id, _)| conn.is_none_or(|conn| conn.entity_id() == **id))
            .filter(|(_, view)| view.read(cx).is_dirty())
            .map(|(_, view)| Unsaved::Notes(view.clone()))
            .collect()
    }

    /// Everything with unsaved changes: file views and notes.
    fn unsaved_everywhere(&self, cx: &App) -> Vec<Unsaved> {
        let mut unsaved = self.unsaved_in(|_| true, cx);
        unsaved.extend(self.unsaved_notes(None, cx));
        unsaved
    }

    /// Runs `then` once the unsaved changes in `views` are taken care of:
    /// right away when there are none, and otherwise after asking, once
    /// they are saved or the user chose to throw them away. Cancelling, or
    /// a save that fails, leaves everything as it was.
    fn guard_unsaved(
        &mut self,
        views: Vec<Unsaved>,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        if views.is_empty() {
            then(self, window, cx);
            return;
        }
        let names: Vec<String> = views.iter().map(|v| v.name(cx)).collect();
        let files = views.iter().all(|v| matches!(v, Unsaved::File(_)));
        let (title, description) = match names.as_slice() {
            [one] if files => (
                format!("Save your changes to {}?", file_name(one)),
                format!("{one} has changes that are not saved."),
            ),
            [_] => (
                "Save your changes to the notes?".to_string(),
                "The agent's notes have changes that are not saved.".to_string(),
            ),
            many if files => (
                format!("Save your changes to {} files?", many.len()),
                format!(
                    "These files have changes that are not saved: {}.",
                    many.join(", ")
                ),
            ),
            many => (
                "Save your changes?".to_string(),
                format!(
                    "These have changes that are not saved: {}.",
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
            let saves: Vec<_> = views.iter().map(|v| v.save(window, cx)).collect();
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
        let unsaved = self.unsaved_everywhere(cx);
        self.guard_unsaved(unsaved, window, cx, |_, _, cx| cx.quit());
    }

    /// Starts `binary`, the updated client, in place of this one, after
    /// asking about unsaved changes.
    pub fn restart(&mut self, binary: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.unsaved_everywhere(cx);
        self.guard_unsaved(unsaved, window, cx, move |ws, _, cx| {
            ws.save_layout(cx);
            cx.set_restart_path(binary);
            cx.restart();
        });
    }

    fn check_for_updates(&mut self, manual: bool, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        crate::ui::update::check(workspace, self.backend.clone(), manual, window, cx);
    }

    fn on_check_for_updates(
        &mut self,
        _: &CheckForUpdates,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_for_updates(true, window, cx);
    }

    /// The title bar's close button, or a close request from the window
    /// manager while there are unsaved changes.
    fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self.unsaved_everywhere(cx);
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
        pred: impl Fn(&TabGroup) -> bool,
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

    /// Archives a session on the agent. The agent then tells every client,
    /// and each of them closes the session's tab.
    fn archive_session(
        &mut self,
        conn: Entity<AgentConnection>,
        id: SessionId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        conn.update(cx, |conn, cx| conn.set_archived(id, true, cx));
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
            notify::info(window, cx, "Open a codespace first.");
            return;
        };
        let conn = group.connection.clone();
        let origin = group.active_pane;
        let read = conn.read(cx);
        let client = read.client.clone();
        let backend = read.backend.clone();
        let owner = read.terminal_owner;
        let task = read
            .backend
            .run(async move { client.open_terminal(80, 24, owner).await });
        cx.spawn_in(window, async move |this, cx: &mut AsyncWindowContext| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Some(Ok(id)) => {
                    let client = conn.read(cx).client.clone();
                    let remote = RemoteTerminal::new(&backend, client, id);
                    let rebind = remote.rebinder();
                    let io: Rc<dyn TerminalIo> = Rc::new(remote);
                    let view = this.terminal_view(Some(io), window, cx);
                    conn.update(cx, |conn, _| {
                        conn.register_terminal(id, &view, rebind, false)
                    });
                    let panel_id = this.alloc_panel();
                    let new_pane = this.alloc_panel();
                    let Some(gix) = this.groups.iter().position(|g| g.is(&conn)) else {
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

    /// Keeps a tab for every terminal of the agent: terminals that another
    /// client opened get one in the agent's active group (or its first),
    /// without taking the focus, and the tabs of terminals that closed on
    /// another client go.
    fn sync_terminal_tabs(
        &mut self,
        conn: &Entity<AgentConnection>,
        closed: &[EntityId],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for entity in closed {
            let panel = self.groups.iter().find_map(|group| {
                group.panes.iter().find_map(|pane| {
                    pane.tabs.iter().find_map(|(id, panel)| match panel {
                        Panel::Terminal(view) if view.entity_id() == *entity => Some(id),
                        _ => None,
                    })
                })
            });
            if let Some(panel) = panel {
                self.close_panel(panel, window, cx);
            }
        }
        let Some(gix) = self
            .active_group
            .filter(|&ix| self.groups.get(ix).is_some_and(|g| g.connection == *conn))
            .or_else(|| self.groups.iter().position(|g| g.connection == *conn))
        else {
            return;
        };
        let terminals = conn.update(cx, |conn, _| conn.untracked_terminals());
        if terminals.is_empty() {
            return;
        }
        let (client, backend) = {
            let read = conn.read(cx);
            (read.client.clone(), read.backend.clone())
        };
        for terminal in terminals {
            let remote = RemoteTerminal::new(&backend, client.clone(), terminal.id);
            let rebind = remote.rebinder();
            let view = self.terminal_view(None, window, cx);
            view.update(cx, |view, cx| {
                view.set_holder(terminal.holder, cx);
                view.connect(Rc::new(remote), cx);
            });
            conn.update(cx, |conn, _| {
                conn.register_terminal(terminal.id, &view, rebind, false)
            });
            let panel_id = self.alloc_panel();
            if let Some(pane) = self.groups[gix].active_mut() {
                pane.tabs.push_background(panel_id, Panel::Terminal(view));
            }
        }
        cx.notify();
    }

    /// Moves a terminal that another client holds, or none does, here, as
    /// the button over its tab asks.
    fn take_over_terminal(&mut self, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let read = view.read(cx);
        if read.exited().is_some() || *read.holder() == TerminalHolder::You {
            return;
        }
        if let Some(conn) = self
            .connections
            .iter()
            .find(|c| c.read(cx).terminal_id(view).is_some())
            .cloned()
        {
            conn.update(cx, |conn, cx| conn.take_terminal(view, cx));
        }
    }

    /// Closes a connection and its tabs.
    fn drop_connection(
        &mut self,
        conn: &Entity<AgentConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_groups(window, cx, |g| g.connection == *conn);
        self.notes.remove(&conn.entity_id());
        self.ports.remove(&conn.entity_id());
        self.connections.retain(|c| c != conn);
        if self.active_connection.as_ref() == Some(conn) {
            self.active_connection = self.connections.first().cloned();
        }
    }

    fn pair_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        open_form(
            "Pair with an oracle",
            Some(
                "Run `loom-oracle pair` on the oracle for the first device, or use Devices > Add device… in a paired desktop client."
                    .into(),
            ),
            vec![
                FormField::new("Name", "home"),
                FormField::new("Pairing link", "loom://oracle.example.com/…"),
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
            oracle: None,
        });
        self.save_config(window, cx);
        self.refresh_host(self.hosts.len() - 1, window, cx);
    }

    fn remove_host(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.hosts.len() {
            let host = self.hosts.remove(ix);
            self.config.appearance.host_colors.remove(host.entry.name());
            self.env.codespaces.set(host.entry.name(), Vec::new());
            if let Some(view) = self.settings.remove(host.entry.name())
                && self.shown_settings.as_ref() == Some(&view)
            {
                self.shown_settings = None;
            }
            self.save_config(window, cx);
            cx.notify();
        }
    }

    /// The color that marks the sessions of the host named `host`.
    fn host_color(&self, host: &str) -> Option<gpui::Hsla> {
        let color = self.config.appearance.host_colors.get(host)?;
        host_colors::parse(color).map(|rgb| gpui::rgb(rgb).into())
    }

    fn set_host_color(
        &mut self,
        ix: usize,
        color: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.hosts.get(ix) else {
            return;
        };
        let name = host.entry.name().to_owned();
        let colors = &mut self.config.appearance.host_colors;
        match color {
            Some(color) => colors.insert(name, color),
            None => colors.remove(&name),
        };
        self.save_config(window, cx);
        cx.notify();
    }

    fn on_toggle_codespaces(
        &mut self,
        _: &ToggleCodespaces,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.show_right && self.right_tab == RightTab::Codespaces {
            self.show_right = false;
        } else {
            self.show_right = true;
            self.right_tab = RightTab::Codespaces;
        }
        cx.notify();
    }

    fn on_show_codespaces(&mut self, _: &ShowCodespaces, _: &mut Window, cx: &mut Context<Self>) {
        self.show_right = true;
        self.right_tab = RightTab::Codespaces;
        cx.notify();
    }

    fn on_toggle_right(&mut self, _: &ToggleRightDock, _: &mut Window, cx: &mut Context<Self>) {
        self.show_right = !self.show_right;
        cx.notify();
    }

    fn session_changes(&mut self, cx: &mut Context<Self>) -> Option<Rc<Vec<FileChange>>> {
        let chat = self.active_group().and_then(|g| g.chat(cx))?;
        let conversation = chat.read(cx).conversation.clone();
        if conversation.read(cx).has_older {
            // The session's changes come from all of its tool calls; fetch the
            // ones before the latest items too.
            let connection = chat.read(cx).connection.clone();
            let id = conversation.read(cx).info.id.clone();
            cx.defer(move |cx| {
                connection.update(cx, |conn, cx| conn.load_all_history(&id, cx));
            });
        }
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
        if let Some((pane, ix)) = group.find_diff(&path, DiffKind::Session, cx) {
            self.activate_tab(pane, ix, window, cx);
            return;
        }
        let Some(chat) = group.chat(cx) else {
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
        committed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = conn.read(cx).info.workspace.join(&change.path);
        if self.active_group().is_none() {
            notify::info(window, cx, "Open a codespace to view diffs.");
            return;
        }
        let kind = if committed {
            DiffKind::Committed
        } else {
            DiffKind::Uncommitted
        };
        if let Some(group) = self.active_group()
            && let Some((pane, ix)) = group.find_diff(&path, kind, cx)
        {
            self.activate_tab(pane, ix, window, cx);
            if let Some(Panel::Diff(view)) = self.active_group().and_then(|g| g.active_panel()) {
                view.update(cx, |view, cx| view.reload(cx));
            }
            return;
        }
        let wrap = self.config.appearance.wrap_lines;
        let view =
            cx.new(|cx| DiffView::git(conn, change.path, change.old_path, committed, wrap, cx));
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

    /// The right panel: tabs for the codespaces, the workspace's changes,
    /// its files, the agent's notes, and the current session's tasks.
    /// Changes, files, and notes need a connection, and tasks a session;
    /// codespaces need neither.
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
                let committed = listing
                    .committed
                    .as_ref()
                    .map(|committed| committed.files.len())
                    .unwrap_or(0);
                (
                    listing.files.len() + committed,
                    listing.branch.clone(),
                    body,
                )
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
        let forwarded = self
            .notes_connection()
            .map(|conn| conn.read(cx).forwards().len())
            .unwrap_or(0);
        let ports_label = if forwarded > 0 {
            format!("Ports {forwarded}")
        } else {
            "Ports".to_string()
        };
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
            (RightTab::Codespaces, _) => self.render_codespaces_panel(cx),
            (RightTab::Notes, _) => match self.notes_view(window, cx) {
                Some(view) => div().flex_1().min_h_0().child(view).into_any_element(),
                None => div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Connect to an agent to see its notes.")
                    .into_any_element(),
            },
            (RightTab::Ports, _) => match self.ports_view(window, cx) {
                Some(view) => div().flex_1().min_h_0().child(view).into_any_element(),
                None => div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Connect to an agent to forward its ports.")
                    .into_any_element(),
            },
            (RightTab::Tasks, _) => match self.tasks_view(window, cx) {
                Some(view) => div().flex_1().min_h_0().child(view).into_any_element(),
                None => div()
                    .p_3()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Open a session to see its tasks.")
                    .into_any_element(),
            },
            (_, Some(tree)) => div()
                .flex_1()
                .min_h_0()
                .child(tree.clone())
                .into_any_element(),
            _ if conn.is_none() => div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Open a codespace in the Codespaces tab to see its changes and files.")
                .into_any_element(),
            _ => body,
        };
        // Scrolls sideways when the panel is too narrow for all the tabs.
        let header = h_flex()
            .id("right-tabs")
            .w_full()
            .h(Tab::container_height())
            .flex_none()
            .overflow_x_scroll()
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("right-codespaces")
                    .debug_selector(|| "right-codespaces".into())
                    .ghost()
                    .xsmall()
                    .label("Codespaces")
                    .selected(tab == RightTab::Codespaces)
                    .tooltip("Codespaces and oracles (ctrl-shift-b)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Codespaces;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("right-changes")
                    .debug_selector(|| "right-changes".into())
                    .ghost()
                    .xsmall()
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
                    .debug_selector(|| "right-files".into())
                    .ghost()
                    .xsmall()
                    .label("Files")
                    .selected(files_tab)
                    .tooltip("Workspace files (ctrl-shift-e)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Files;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("right-notes")
                    .debug_selector(|| "right-notes".into())
                    .ghost()
                    .xsmall()
                    .label("Notes")
                    .selected(tab == RightTab::Notes)
                    .tooltip("The agent's notes, kept by its sessions and you (ctrl-shift-n)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Notes;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("right-tasks")
                    .debug_selector(|| "right-tasks".into())
                    .ghost()
                    .xsmall()
                    .label("Tasks")
                    .selected(tab == RightTab::Tasks)
                    .tooltip("The session's task list, kept by the agent and you (ctrl-shift-t)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Tasks;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("right-ports")
                    .debug_selector(|| "right-ports".into())
                    .ghost()
                    .xsmall()
                    .label(ports_label)
                    .selected(tab == RightTab::Ports)
                    .tooltip("Ports on the agent's host, and the local ports forwarded to them")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_tab = RightTab::Ports;
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
                let upload = tree.clone();
                el.child(
                    Button::new("upload-files")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(gpui_kit_assets::IconName::Upload))
                        .tooltip("Upload files to the selected folder")
                        .on_click(cx.listener(move |_, _, window, cx| {
                            upload.update(cx, |tree, cx| {
                                tree.prompt_upload(tree.upload_folder(), window, cx)
                            })
                        })),
                )
                .child(
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
            .when(tab == RightTab::Codespaces, |el| {
                el.child(
                    Button::new("new-codespace")
                        .debug_selector(|| "new-codespace".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip("New codespace (ctrl-alt-n)")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.workspace_picker(window, cx)),
                        ),
                )
                .child(
                    Button::new("pair")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Network)
                        .tooltip("Pair with an oracle")
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
        let committed = listing
            .committed
            .as_ref()
            .filter(|committed| !committed.commits.is_empty() || !committed.files.is_empty());
        if listing.files.is_empty() && committed.is_none() {
            return div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No uncommitted changes.")
                .into_any_element();
        }
        let touched: HashSet<PathBuf> = session
            .map(|changes| changes.iter().map(|c| c.path.clone()).collect())
            .unwrap_or_default();
        let heading = |text: String| {
            div()
                .px_3()
                .pt_2()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let note = |text: String| {
            div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let mut list = v_flex()
            .id("changes-list")
            .flex_1()
            .overflow_y_scroll()
            .py_1();
        if committed.is_some() {
            list = list.child(heading("Uncommitted".into()));
            if listing.files.is_empty() {
                list = list.child(note("No uncommitted changes.".into()));
            }
        }
        list = list
            .children(self.git_change_rows(&conn, &listing.files, false, 0, &touched, cx))
            .when(listing.truncated, |el| {
                el.child(note(format!(
                    "Showing the first {} files.",
                    listing.files.len()
                )))
            });
        if let Some(committed) = committed {
            let commits = match committed.commits.len() {
                1 => "1 commit".to_string(),
                n => format!("{n} commits"),
            };
            list = list
                .child(heading(format!(
                    "Committed since {} ({commits})",
                    committed.base
                )))
                .children(committed.commits.iter().map(|commit| {
                    h_flex()
                        .px_3()
                        .py_0p5()
                        .gap_2()
                        .text_xs()
                        .whitespace_nowrap()
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .child(short_id(&commit.id).to_owned()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(commit.summary.clone()),
                        )
                }))
                .child(div().h_1())
                .children(self.git_change_rows(
                    &conn,
                    &committed.files,
                    true,
                    listing.files.len() + 1,
                    &touched,
                    cx,
                ))
                .when(committed.truncated, |el| {
                    el.child(note(
                        "There are too many commits or files to show them all.".into(),
                    ))
                });
        }
        list.into_any_element()
    }

    /// The rows of files that git lists as changed; a click opens the
    /// file's uncommitted diff, or with `committed`, its committed diff.
    fn git_change_rows(
        &self,
        conn: &Entity<AgentConnection>,
        files: &[GitChange],
        committed: bool,
        first_ix: usize,
        touched: &HashSet<PathBuf>,
        cx: &Context<Self>,
    ) -> Vec<gpui::Stateful<gpui::Div>> {
        let theme = cx.theme();
        let workspace = conn.read(cx).info.workspace.clone();
        let kind = if committed {
            DiffKind::Committed
        } else {
            DiffKind::Uncommitted
        };
        let open = self.open_diff_path(kind, cx);
        let directories = self.codespace_directories(conn, cx);
        let mut last_repository = None;
        let mut rows = Vec::new();
        for (ix, change) in files.iter().enumerate() {
            let repository = loom_view_model::codespaces::repository_of(&change.path, &directories);
            if repository.is_some() && repository != last_repository {
                rows.push(
                    h_flex()
                        .id(SharedString::from(format!(
                            "changes-repository-{committed}-{ix}"
                        )))
                        .px_3()
                        .pt_1()
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(Icon::new(gpui_kit_assets::IconName::FolderGit2).xsmall())
                        .child(repository.unwrap_or_default().to_owned()),
                );
                last_repository = repository;
            }
            rows.push({
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
                    ix: first_ix + ix,
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
                row.render(theme)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_git_diff(conn.clone(), change.clone(), committed, window, cx)
                    }))
            });
        }
        rows
    }

    fn open_diff_path(&self, kind: DiffKind, cx: &App) -> Option<PathBuf> {
        self.active_group()
            .and_then(|g| g.active_panel())
            .and_then(|panel| match panel {
                Panel::Diff(view) if view.read(cx).kind() == kind => {
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
        let open = self.open_diff_path(DiffKind::Session, cx);
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
            None if !self.hosts.is_empty() => self.workspace_picker(window, cx),
            None => notify::info(window, cx, "Pair with an oracle first."),
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
        if let Some(group) = self.active_group()
            && let Some(id) = group.session.clone()
        {
            let conn = group.connection.clone();
            self.archive_session(conn, id, window, cx);
        }
    }

    fn on_resume_session(
        &mut self,
        _: &ResumeSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.changes_connection() {
            Some(conn) => {
                let workspace = cx.entity().downgrade();
                crate::ui::resume::open(conn, workspace, window, cx)
            }
            None => notify::info(window, cx, "Open a codespace to resume a session in it."),
        }
    }

    fn on_rename_session(
        &mut self,
        _: &RenameSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.active_group().and_then(|g| g.chat(cx)) else {
            notify::info(window, cx, "Open a session to rename it.");
            return;
        };
        let chat = chat.read(cx);
        let conn = chat.connection.clone();
        let conversation = chat.conversation.read(cx);
        let (id, title) = (conversation.info.id.clone(), conversation.title());
        crate::ui::session::open_rename(conn, id, title, window, cx);
    }

    fn on_show_system_prompt(
        &mut self,
        _: &ShowSystemPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .active_group()
            .is_none_or(|group| group.session.is_none())
        {
            notify::info(window, cx, "Open a session to see its system prompt.");
            return;
        }
        self.show_system_prompt(window, cx);
    }

    /// Shows the active session's system prompt tab, read again, or opens
    /// one.
    fn show_system_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.active_group() else {
            return;
        };
        let Some(session) = group.session.clone() else {
            return;
        };
        let found = group.panes.iter().find_map(|pane| {
            pane.tabs
                .find(|panel| {
                    matches!(panel, Panel::SystemPrompt(view) if *view.read(cx).session() == session)
                })
                .map(|ix| (pane.id, ix))
        });
        if let Some((pane, ix)) = found {
            self.activate_tab(pane, ix, window, cx);
            if let Some(Panel::SystemPrompt(view)) =
                self.active_group().and_then(|g| g.active_panel())
            {
                view.update(cx, |view, cx| view.reload(window, cx));
            }
            return;
        }
        let conn = group.connection.clone();
        let panel = self.system_prompt_panel(&conn, session, window, cx);
        let id = self.alloc_panel();
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut()) {
            pane.tabs.add(id, panel, None);
        }
        self.focus_active_panel(window, cx);
        cx.notify();
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

    /// Shows the tab at `ix` of the active pane, or its last tab for `None`.
    /// Does nothing when the pane has no tab there.
    fn activate_tab_in_active_pane(
        &mut self,
        ix: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.active_group().and_then(|g| g.active()) else {
            return;
        };
        let Some(ix) = ix.or_else(|| pane.tabs.len().checked_sub(1)) else {
            return;
        };
        let id = pane.id;
        self.activate_tab(id, ix, window, cx);
    }

    fn on_move_tab_left(&mut self, _: &MoveTabLeft, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut())
            && pane.tabs.move_active_left()
        {
            self.focus_active_panel(window, cx);
            cx.notify();
        }
    }

    fn on_move_tab_right(&mut self, _: &MoveTabRight, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.active_group_mut().and_then(|g| g.active_mut())
            && pane.tabs.move_active_right()
        {
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
        if let Some(group) = self.active_group()
            && let Some(id) = group.session.clone()
        {
            let conn = group.connection.clone();
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
        for conn in &self.connections {
            let agent = conn.read(cx).title();
            for info in conn.read(cx).sessions.iter().filter(|s| !s.archived) {
                let title = info
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("Session {}", short_id(&info.id.0)));
                let (conn, info2, workspace) = (conn.clone(), info.clone(), workspace.clone());
                let item = PickerItem::new(title, move |window, cx| {
                    let (conn, info) = (conn.clone(), info2.clone());
                    let _ = workspace.update(cx, |ws, cx| ws.open_session(conn, info, window, cx));
                });
                items.push(item.detail(agent.clone()));
            }
        }
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
                            None => ws.connect(hix, instance, window, cx),
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

    fn on_pair(&mut self, _: &PairOracle, window: &mut Window, cx: &mut Context<Self>) {
        self.pair_dialog(window, cx);
    }

    /// The button that picks the color that marks a host's sessions.
    fn render_host_color_menu(&self, hix: usize, cx: &mut Context<Self>) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let current = self
            .hosts
            .get(hix)
            .and_then(|h| self.config.appearance.host_colors.get(h.entry.name()))
            .and_then(|c| host_colors::parse(c));
        Button::new(("host-color", hix))
            .ghost()
            .xsmall()
            .icon(gpui_kit_assets::IconName::Palette)
            .tooltip("Color of this host's sessions")
            .dropdown_menu(move |mut menu, _, _| {
                let pick = |color: Option<&str>| {
                    let workspace = workspace.clone();
                    let color = color.map(str::to_owned);
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = workspace.update(cx, |ws, cx| {
                            ws.set_host_color(hix, color.clone(), window, cx)
                        });
                    }
                };
                for (label, hex) in host_colors::PALETTE {
                    let rgb = host_colors::parse(hex).unwrap_or_default();
                    let swatch: gpui::Hsla = gpui::rgb(rgb).into();
                    let item = PopupMenuItem::element(move |_, _| {
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size(px(10.)).rounded_full().bg(swatch))
                            .child(label)
                    });
                    menu = menu.item(item.checked(current == Some(rgb)).on_click(pick(Some(hex))));
                }
                menu.separator().item(
                    PopupMenuItem::new("None")
                        .checked(current.is_none())
                        .on_click(pick(None)),
                )
            })
            .into_any_element()
    }

    fn render_center(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        if let Some(settings) = self.shown_settings.clone() {
            return self.render_settings(settings, cx);
        }
        let Some(group) = self.active_group() else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(div().text_xl().child("Loom"))
                .child("Open a codespace in the Codespaces tab on the right, or start one with + there.")
                .child(
                    div().text_sm().child(
                        "ctrl-shift-p commands · ctrl-p sessions · ctrl-n new session · ctrl-` terminal · ctrl-b codespaces panel",
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
        let connection = self.active_group().map(|g| g.connection.clone());
        type TabInfo = (
            PanelId,
            PanelKind,
            String,
            bool,
            bool,
            Option<(SessionId, SessionState)>,
        );
        let panels: Vec<TabInfo> = pane
            .tabs
            .iter()
            .map(|(id, panel)| {
                let unsaved = panel.unsaved(cx).is_some();
                let closable = panel.closable(cx);
                let session = match panel {
                    Panel::Chat(view) => {
                        let conversation = view.read(cx).conversation.read(cx);
                        Some((conversation.info.id.clone(), conversation.state))
                    }
                    _ => None,
                };
                (
                    id,
                    panel.kind(),
                    panel.title(cx),
                    unsaved,
                    closable,
                    session,
                )
            })
            .collect();
        let content = pane.tabs.active().cloned();
        let dim = multiple && !is_active;
        let workspace = cx.entity().downgrade();
        let tabs = panels.into_iter().enumerate().map(
            |(ix, (id, kind, title, unsaved, closable, session))| {
                let dragged = DraggedTab {
                    pane: pane_id,
                    panel: id,
                    ix,
                    kind,
                    title: title.clone().into(),
                };
                let icon = match &session {
                    Some((_, state)) => session_icon(*state, &theme),
                    None => panel_icon(kind).into_any_element(),
                };
                let label = div()
                    .id(("tab-label", id as usize))
                    .max_w(px(240.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .when(dim, |el| el.text_color(theme.muted_foreground))
                    .child(title.clone());
                let label = match (session, connection.clone()) {
                    (Some((session, _)), Some(conn)) => {
                        let workspace = workspace.clone();
                        label
                            .context_menu(move |menu, _, _| {
                                session_tab_menu(
                                    menu,
                                    workspace.clone(),
                                    conn.clone(),
                                    session.clone(),
                                    title.clone(),
                                )
                            })
                            .into_any_element()
                    }
                    _ => label.into_any_element(),
                };
                Tab::new(("tab", id as usize))
                    .position(Tab::position_for(ix, len, active))
                    .selected(ix == active)
                    .start_slot(icon)
                    .child(label)
                    .when(unsaved, |tab| {
                        tab.child(
                            div()
                                .debug_selector(move || format!("tab-unsaved-{id}"))
                                .text_color(theme.muted_foreground)
                                .child("●"),
                        )
                    })
                    .when(closable, |tab| {
                        tab.end_slot(
                            Button::new(("close-tab", id as usize))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(pane_id, ix, window, cx)
                                })),
                        )
                    })
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
            },
        );
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
                        Button::new(("new-tab", pane_id as usize))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("New session or terminal")
                            .dropdown_menu(move |menu, _, _| {
                                let new_session = workspace.clone();
                                let new_terminal = workspace.clone();
                                menu.item(PopupMenuItem::new("New session").on_click(
                                    move |_, window, cx| {
                                        let _ = new_session.update(cx, |ws, cx| {
                                            if let Some(conn) =
                                                ws.active_group().map(|g| g.connection.clone())
                                            {
                                                ws.new_session(conn, window, cx);
                                            }
                                        });
                                    },
                                ))
                                .item(
                                    PopupMenuItem::new("New terminal").on_click(
                                        move |_, window, cx| {
                                            let _ = new_terminal.update(cx, |ws, cx| {
                                                ws.set_active_pane(pane_id, cx);
                                                ws.new_terminal(None, window, cx)
                                            });
                                        },
                                    ),
                                )
                            }),
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
            let (transport, transport_icon) =
                ("oracle relay (end-to-end encrypted)", IconName::Globe);
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
        if let Some(tab) = self.active_group().and_then(|g| g.chat(cx)) {
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
            .on_action(cx.listener(Self::on_toggle_codespaces))
            .on_action(cx.listener(Self::on_toggle_right))
            .on_action(cx.listener(Self::on_command_palette))
            .on_action(cx.listener(Self::on_find_session))
            .on_action(cx.listener(Self::on_find_agent))
            .on_action(cx.listener(Self::on_check_for_updates))
            .on_action(cx.listener(Self::on_select_theme))
            .on_action(cx.listener(Self::on_new_session))
            .on_action(cx.listener(Self::on_new_codespace))
            .on_action(cx.listener(Self::on_oracle_settings))
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_archive_session))
            .on_action(cx.listener(Self::on_resume_session))
            .on_action(cx.listener(Self::on_rename_session))
            .on_action(cx.listener(Self::on_show_system_prompt))
            .on_action(cx.listener(Self::on_show_changes))
            .on_action(cx.listener(Self::on_show_files))
            .on_action(cx.listener(Self::on_show_notes))
            .on_action(cx.listener(Self::on_show_ports))
            .on_action(cx.listener(Self::on_show_tasks))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_previous_tab))
            .on_action(cx.listener(|this, _: &ActivateTab1, window, cx| {
                this.activate_tab_in_active_pane(Some(0), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab2, window, cx| {
                this.activate_tab_in_active_pane(Some(1), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab3, window, cx| {
                this.activate_tab_in_active_pane(Some(2), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab4, window, cx| {
                this.activate_tab_in_active_pane(Some(3), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab5, window, cx| {
                this.activate_tab_in_active_pane(Some(4), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab6, window, cx| {
                this.activate_tab_in_active_pane(Some(5), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab7, window, cx| {
                this.activate_tab_in_active_pane(Some(6), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateTab8, window, cx| {
                this.activate_tab_in_active_pane(Some(7), window, cx)
            }))
            .on_action(cx.listener(|this, _: &ActivateLastTab, window, cx| {
                this.activate_tab_in_active_pane(None, window, cx)
            }))
            .on_action(cx.listener(Self::on_move_tab_left))
            .on_action(cx.listener(Self::on_move_tab_right))
            .on_action(cx.listener(Self::on_show_codespaces))
            .on_action(cx.listener(Self::on_providers))
            .on_action(cx.listener(Self::on_choose_model))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Toast {
    Info,
    Success,
    Error,
}

/// How to tell the user of a notice from the agent at `title`.
fn notice_toast(title: &str, notice: &Notice) -> (Toast, String) {
    match notice {
        Notice::ConfigReloaded { restart_required } if restart_required.is_empty() => (
            Toast::Success,
            format!("{title}: reloaded the configuration"),
        ),
        Notice::ConfigReloaded { restart_required } => (
            Toast::Info,
            format!(
                "{title}: reloaded the configuration; restart the agent to apply {}",
                restart_required.join(", ")
            ),
        ),
        // The rest of the message points at the line in the file, which a
        // toast has no room for.
        Notice::ConfigRejected { message } => (
            Toast::Error,
            format!(
                "{title}: the configuration was rejected, so the agent keeps its old one: {}",
                message.lines().next().unwrap_or_default()
            ),
        ),
        Notice::ShuttingDown => (Toast::Info, format!("{title}: the agent is shutting down")),
        other => (Toast::Info, format!("{title}: {other:?}")),
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
        let (ws, conn, cx) = connected(agent, cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.new_session(conn.clone(), window, cx)
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

    /// A workspace in a window, connected to `agent`.
    fn connected<'a>(
        agent: &TestAgent,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<AgentConnection>,
        &'a mut VisualTestContext,
    ) {
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
            codespaces: Default::default(),
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
        });
        (ws, conn, cx)
    }

    #[test]
    fn config_notices_read_as_sentences() {
        let title = "local · /work";
        assert_eq!(
            notice_toast(
                title,
                &Notice::ConfigReloaded {
                    restart_required: vec![]
                }
            ),
            (
                Toast::Success,
                "local · /work: reloaded the configuration".into()
            )
        );
        assert_eq!(
            notice_toast(
                title,
                &Notice::ConfigReloaded {
                    restart_required: vec!["socket".into(), "storage".into()]
                }
            ),
            (
                Toast::Info,
                "local · /work: reloaded the configuration; \
                 restart the agent to apply socket, storage"
                    .into()
            )
        );
        assert_eq!(
            notice_toast(
                title,
                &Notice::ConfigRejected {
                    message: "agent.yaml: error: unknown field `bogus`\n --> <input>:3:1".into()
                }
            ),
            (
                Toast::Error,
                "local · /work: the configuration was rejected, so the agent keeps its \
                 old one: agent.yaml: error: unknown field `bogus`"
                    .into()
            )
        );
        assert_eq!(
            notice_toast(title, &Notice::ShuttingDown).1,
            "local · /work: the agent is shutting down"
        );
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

    #[gpui::test]
    fn an_oracle_shows_its_workspaces_and_opens_new_codespace(cx: &mut TestAppContext) {
        use crate::oracle::tests::{TestOracle, wait};
        use loom_client::oracle_proto::{Repository, Role};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let test = TestOracle::start(&root);
        let _host = test.pair_host("box");
        let host = test.oracle.store().peers(Some(Role::Host)).unwrap()[0].id;
        let repository = |url: &str| Repository {
            url: url.into(),
            provider: None,
            directory: None,
            base_branch: None,
        };
        test.oracle
            .store()
            .create_workspace(
                "acme",
                host,
                &[
                    repository("https://github.com/acme/server.git"),
                    repository("https://github.com/acme/web.git"),
                ],
                &[],
            )
            .unwrap();
        test.oracle.set_preset("dev", "", None, None, None).unwrap();

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let config = ClientConfig {
            hosts: vec![test.entry.clone()],
            ..Default::default()
        };
        let (env, backend) = (test.env.clone(), test.backend.clone());
        let slot = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let slot = slot.clone();
            let config_path = root.join("client.yaml");
            move |window, cx| {
                let ws = cx.new(|cx| Workspace::new(backend, env, config, config_path, window, cx));
                *slot.borrow_mut() = Some(ws.clone());
                Root::new(ws, window, cx)
            }
        });
        let ws: Entity<Workspace> = slot.borrow_mut().take().unwrap();
        wait(cx, |cx| {
            ws.read(cx).hosts[0].oracle.as_ref().is_some_and(|link| {
                !link.read(cx).workspaces.is_empty() && !link.read(cx).presets.is_empty()
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("oracle-0").is_some());
        assert!(cx.debug_bounds("codespace-0").is_none());
        assert!(!has_dialog(cx));

        assert!(cx.debug_bounds("new-codespace").is_some());

        let link = ws.read_with(cx, |ws, _| ws.hosts[0].oracle.clone().unwrap());
        ws.update_in(cx, |ws, window, cx| {
            ws.new_codespace_dialog(link.clone(), "acme", window, cx)
        });
        cx.run_until_parked();
        assert!(has_dialog(cx));
        assert!(cx.debug_bounds("create-codespace").is_some());
        cx.update(|window, cx| window.close_dialog(cx));
        cx.run_until_parked();

        cx.dispatch_action(NewCodespace);
        cx.run_until_parked();
        assert!(has_dialog(cx));
        cx.update(|window, cx| window.close_dialog(cx));
        cx.run_until_parked();

        cx.dispatch_action(OracleSettings);
        cx.run_until_parked();
        assert!(ws.read_with(cx, |ws, _| ws.shown_settings.is_some()));
        assert!(cx.debug_bounds("settings-page-workspaces").is_some());
        click("settings-page-workspaces", cx);
        assert!(cx.debug_bounds("settings-workspace-0").is_some());
        ws.update_in(cx, |ws, window, cx| ws.close_settings(window, cx));
        assert!(ws.read_with(cx, |ws, _| ws.shown_settings.is_none()));
        assert!(ws.read_with(cx, |ws, _| ws.settings.len() == 1));
    }

    #[gpui::test]
    fn the_codespaces_panel_lists_codespaces_and_their_sessions(cx: &mut TestAppContext) {
        use crate::oracle::tests::{TestOracle, wait};
        use loom_client::oracle_proto::{CodespaceState, Role, SessionSummary};
        use loom_oracle::store::CodespaceRow;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let test = TestOracle::start(&root);
        let _host = test.pair_host("box");
        let store = test.oracle.store();
        let host = store.peers(Some(Role::Host)).unwrap()[0].id;
        let workspace = store.create_workspace("acme", host, &[], &[]).unwrap();
        let row = CodespaceRow {
            id: "k3x9".into(),
            workspace: workspace.id,
            workspace_name: "acme".into(),
            host,
            host_name: "box".into(),
            preset: None,
            preset_name: None,
            peer: None,
            state: CodespaceState::Stopped,
            cpus: 2,
            memory_mib: 4096,
            instance: "loom-k3x9".into(),
            branch: "loom/k3x9".into(),
            base_branch: None,
            repositories: Vec::new(),
            client_version: None,
            agent: None,
            step: None,
            error: None,
            first_message: None,
            created_at: 1,
            updated_at: 1,
            stopped_at: None,
            active: None,
            active_at: None,
            metrics: None,
            archive: None,
        };
        store.insert_codespace(&row, &[]).unwrap();
        let closed = CodespaceRow {
            id: "c0de".into(),
            state: CodespaceState::Closed,
            ..row.clone()
        };
        store.insert_codespace(&closed, &[]).unwrap();
        let summary = SessionSummary {
            id: "s1".into(),
            title: Some("Fix the build".into()),
            state: "running".into(),
            archived: false,
            model: None,
            created_at: 1,
            updated_at: 2,
        };
        store
            .put_sessions("k3x9", std::slice::from_ref(&summary), &[], true)
            .unwrap();

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::ui::bind_keys(&Default::default(), cx);
        });
        let config = ClientConfig {
            hosts: vec![test.entry.clone()],
            ..Default::default()
        };
        let (env, backend) = (test.env.clone(), test.backend.clone());
        let slot = Rc::new(RefCell::new(None));
        let (_, cx) = cx.add_window_view({
            let slot = slot.clone();
            let config_path = root.join("client.yaml");
            move |window, cx| {
                let ws = cx.new(|cx| Workspace::new(backend, env, config, config_path, window, cx));
                *slot.borrow_mut() = Some(ws.clone());
                Root::new(ws, window, cx)
            }
        });
        let ws: Entity<Workspace> = slot.borrow_mut().take().unwrap();
        wait(cx, |cx| {
            ws.read(cx).hosts[0].oracle.as_ref().is_some_and(|link| {
                let link = link.read(cx);
                link.codespaces.len() == 2 && !link.sessions.is_empty()
            })
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("codespace-0").is_some());
        assert!(cx.debug_bounds("codespace-1").is_none());
        assert!(cx.debug_bounds("oracle-0").is_some());
        let infos = ws.read_with(cx, |ws, cx| {
            let link = ws.hosts[0].oracle.clone().unwrap();
            let link = link.read(cx);
            let codespace = link.codespaces.iter().find(|c| c.id == "k3x9").unwrap();
            ws.codespace_session_infos(link, codespace, cx)
        });
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].title.as_deref(), Some("Fix the build"));
    }

    #[test]
    fn keymaps_naming_the_old_panel_actions_still_work() {
        assert_eq!(ToggleCodespaces.name(), "loom::ToggleCodespaces");
        assert_eq!(
            ToggleCodespaces::deprecated_aliases(),
            [
                "loom::ToggleSessions",
                "loom::ToggleLeftDock",
                "loom::ToggleAgents"
            ]
        );
        assert_eq!(
            ShowCodespaces::deprecated_aliases(),
            ["loom::ShowSessions", "loom::ShowAgents"]
        );
        assert_eq!(ArchiveSession::deprecated_aliases(), ["loom::CloseSession"]);
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
    fn archiving_a_session_closes_only_its_tab(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        cx.simulate_input("x");
        let (conn, session) = ws.read_with(cx, |ws, _| {
            let group = ws.active_group().unwrap();
            (group.connection.clone(), group.session.clone().unwrap())
        });
        ws.read_with(cx, |ws, cx| {
            assert!(ws.groups[0].find_chat(&session, cx).is_some());
        });
        ws.update_in(cx, |ws, window, cx| {
            ws.archive_session(conn.clone(), session.clone(), window, cx)
        });
        assert!(!has_dialog(cx));
        wait_until(cx, |cx| {
            ws.read(cx).groups[0].find_chat(&session, cx).is_none()
        });
        assert_eq!(ws.read_with(cx, |ws, _| ws.groups.len()), 1);
        assert!(ws.read_with(cx, |ws, _| ws.groups[0].session.is_none()));
        assert!(cx.update(|_, cx| file_view(&ws, cx)).is_some());
    }

    #[gpui::test]
    fn a_codespace_group_has_a_tab_for_each_session_across_restarts(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, conn, cx) = connected(&agent, cx);
        for _ in 0..2 {
            ws.update_in(cx, |ws, window, cx| {
                ws.new_session(conn.clone(), window, cx)
            });
        }
        wait_until(cx, |cx| {
            ws.read(cx)
                .groups
                .first()
                .is_some_and(|g| g.chats().count() == 2)
        });
        assert_eq!(ws.read_with(cx, |ws, _| ws.groups.len()), 1);
        let current = ws.read_with(cx, |ws, _| ws.groups[0].session.clone().unwrap());

        let (phone, _phone_notes) = agent.client("loom-android");
        let added = agent.block_on({
            let phone = phone.clone();
            async move { phone.create_session(None).await.unwrap() }
        });
        wait_until(cx, |cx| ws.read(cx).groups[0].chats().count() == 3);
        ws.read_with(cx, |ws, cx| {
            assert_eq!(ws.groups[0].session.as_ref(), Some(&current));
            match ws.groups[0].active_panel() {
                Some(Panel::Chat(view)) => assert_eq!(view.read(cx).session_id(cx), current),
                _ => panic!("the session tab lost the focus"),
            }
        });
        agent.block_on({
            let phone = phone.clone();
            let id = added.id.clone();
            async move { phone.archive_session(&id, true).await.unwrap() }
        });
        wait_until(cx, |cx| ws.read(cx).groups[0].chats().count() == 2);

        let saved = ws.read_with(cx, |ws, cx| ws.saved_group(&ws.groups[0], cx).unwrap());
        let layout::SavedMember::Pane(pane) = &saved.root else {
            panic!("one pane, not {:?}", saved.root);
        };
        let sessions: Vec<String> = pane
            .tabs
            .iter()
            .filter_map(|tab| match tab {
                SavedTab::Conversation { session } => Some(session.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains(&current.0));
        assert_eq!(
            saved.key.workspace,
            conn.read_with(cx, |conn, _| conn.info.workspace.clone())
        );

        ws.update_in(cx, |ws, window, cx| {
            ws.remove_groups(window, cx, |_| true);
            let sessions = conn.read(cx).sessions.clone();
            let group = ws
                .restore_group(&conn, &sessions, &saved, window, cx)
                .unwrap();
            assert_eq!(group.chats().count(), 2);
            assert_eq!(group.session.as_ref(), Some(&current));
            let gone = layout::SavedTab::Conversation {
                session: added.id.0.clone(),
            };
            assert!(
                ws.restore_panel(&conn, &sessions, &gone, window, cx)
                    .is_none()
            );
        });
    }

    #[gpui::test]
    fn the_resume_dialog_opens_an_archived_session_as_a_tab(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, conn, cx) = connected(&agent, cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.new_session(conn.clone(), window, cx)
        });
        wait_until(cx, |cx| ws.read(cx).active_group.is_some());
        let archived = agent.archived_session("s-old", "Fix the parser");
        wait_until(cx, |cx| {
            conn.read(cx)
                .sessions
                .iter()
                .any(|s| s.id == archived && s.archived)
        });
        assert!(ws.read_with(cx, |ws, cx| ws.groups[0].find_chat(&archived, cx).is_none()));

        cx.dispatch_action(ResumeSession);
        for _ in 0..1000 {
            cx.run_until_parked();
            if has_dialog(cx) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(has_dialog(cx));
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| {
            ws.read(cx).groups[0].session.as_ref() == Some(&archived)
        });
        ws.read_with(cx, |ws, cx| {
            assert!(ws.groups[0].find_chat(&archived, cx).is_some());
            assert_eq!(ws.groups[0].chats().count(), 2);
        });
    }

    /// The active agent's notes view, once the Notes tab has shown it.
    fn notes_view(ws: &Entity<Workspace>, cx: &App) -> Option<Entity<NotesView>> {
        let ws = ws.read(cx);
        let conn = ws.notes_connection()?;
        ws.notes.get(&conn.entity_id()).cloned()
    }

    /// The active session's tasks view, once the Tasks tab has shown it.
    fn tasks_view(ws: &Entity<Workspace>, cx: &App) -> Option<Entity<TasksView>> {
        ws.read(cx).active_group().and_then(|g| g.tasks.clone())
    }

    /// Shows the Notes tab and types `text` into the notes.
    fn edit_notes(
        ws: &Entity<Workspace>,
        text: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<NotesView> {
        click("right-notes", cx);
        let view = cx
            .update(|_, cx| notes_view(ws, cx))
            .expect("no notes view");
        wait_until(cx, |cx| view.read(cx).max_bytes() > 0);
        view.update_in(cx, |view, window, cx| {
            view.editor_focus(cx).focus(window, cx)
        });
        cx.simulate_input(text);
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        view
    }

    #[gpui::test]
    fn the_tasks_tab_follows_the_active_session_and_the_notes_tab_its_agent(
        cx: &mut TestAppContext,
    ) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        click("right-tasks", cx);
        let first_tasks = cx
            .update(|_, cx| tasks_view(&ws, cx))
            .expect("no tasks view");
        let first = edit_notes(&ws, "plan", cx);
        let (conn, session) = ws.read_with(cx, |ws, _| {
            let group = ws.active_group().unwrap();
            (group.connection.clone(), group.session.clone().unwrap())
        });
        assert_eq!(
            first.read_with(cx, |view, _| view.connection().clone()),
            conn
        );

        ws.update_in(cx, |ws, window, cx| {
            ws.new_session(conn.clone(), window, cx)
        });
        wait_until(cx, |cx| {
            ws.read(cx)
                .active_group()
                .is_some_and(|g| g.session.as_ref() != Some(&session))
        });
        cx.run_until_parked();
        // The sessions of one agent share its notes, edits and all.
        let second = cx
            .update(|_, cx| notes_view(&ws, cx))
            .expect("no notes view");
        assert_eq!(second, first);
        assert!(first.read_with(cx, |view, _| view.is_dirty()));
        let other = ws.read_with(cx, |ws, _| {
            ws.active_group().unwrap().session.clone().unwrap()
        });
        click("right-tasks", cx);
        let second_tasks = cx
            .update(|_, cx| tasks_view(&ws, cx))
            .expect("no tasks view");
        assert_ne!(second_tasks, first_tasks);
        assert_eq!(
            second_tasks.read_with(cx, |view, _| view.session().clone()),
            other
        );
        let _ = first_tasks;
        assert_ne!(other, session);
    }

    #[gpui::test]
    fn archiving_a_session_keeps_the_agents_unsaved_notes(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        let view = edit_notes(&ws, "plan", cx);

        assert!(!cx.simulate_close());
        assert!(has_dialog(cx));
        click("unsaved-cancel", cx);

        // Archiving the session does not ask about the notes, which belong to
        // the agent and stay.
        let (conn, session) = ws.read_with(cx, |ws, _| {
            let group = ws.active_group().unwrap();
            (group.connection.clone(), group.session.clone().unwrap())
        });
        ws.update_in(cx, |ws, window, cx| {
            ws.archive_session(conn, session, window, cx)
        });
        assert!(!has_dialog(cx));
        wait_until(cx, |cx| ws.read(cx).groups[0].chats().next().is_none());
        assert_eq!(cx.update(|_, cx| notes_view(&ws, cx)), Some(view.clone()));
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        let saved = view.update_in(cx, |view, window, cx| view.save(window, cx));
        wait_until(cx, |cx| !view.read(cx).is_saving());
        assert!(futures::executor::block_on(saved));
        let conn = view.read_with(cx, |view, _| view.connection().clone());
        let task = conn.read_with(cx, |conn, _| conn.read_notes());
        assert_eq!(
            futures::executor::block_on(task).unwrap().unwrap().text,
            "plan"
        );
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

    /// The panel ids of the active pane's tabs, and the active tab's index.
    fn active_tabs(ws: &Entity<Workspace>, cx: &mut VisualTestContext) -> (Vec<PanelId>, usize) {
        ws.read_with(cx, |ws, _| {
            let pane = ws.active_group().unwrap().active().unwrap();
            (
                pane.tabs.iter().map(|(id, _)| id).collect(),
                pane.tabs.active_index(),
            )
        })
    }

    /// A workspace whose active pane has the conversation and three file
    /// tabs, with the last file focused.
    fn open_three_files<'a>(
        agent: &TestAgent,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        for name in ["a.txt", "b.txt", "c.txt"] {
            agent.write(name, "one\n");
        }
        let (ws, cx) = open_file(agent, "a.txt", cx);
        for name in ["b.txt", "c.txt"] {
            ws.update_in(cx, |ws, window, cx| {
                let conn = ws.active_group().unwrap().connection.clone();
                ws.open_file(conn, name.into(), window, cx)
            });
        }
        cx.run_until_parked();
        assert_eq!(active_tabs(&ws, cx).0.len(), 4);
        (ws, cx)
    }

    #[gpui::test]
    fn tab_shortcuts_switch_and_move_tabs(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, cx) = open_three_files(&agent, cx);
        let (ids, active) = active_tabs(&ws, cx);
        assert_eq!(active, 3);

        let active_after = |keys: &str, cx: &mut VisualTestContext| {
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            active_tabs(&ws, cx).1
        };
        assert_eq!(active_after("alt-1", cx), 0);
        assert_eq!(active_after("alt-9", cx), 3);
        assert_eq!(active_after("alt-2", cx), 1);
        // There is no fifth tab, so alt-5 leaves the active one alone.
        assert_eq!(active_after("alt-5", cx), 1);
        assert_eq!(active_after("ctrl-tab", cx), 2);
        assert_eq!(active_after("ctrl-shift-tab", cx), 1);
        assert_eq!(active_after("ctrl-shift-tab", cx), 0);
        assert_eq!(active_after("ctrl-shift-tab", cx), 3);
        assert_eq!(active_after("ctrl-tab", cx), 0);

        assert_eq!(active_after("alt-2", cx), 1);
        assert_eq!(active_after("ctrl-shift-pagedown", cx), 2);
        assert_eq!(active_tabs(&ws, cx).0, [ids[0], ids[2], ids[1], ids[3]]);
        assert_eq!(active_after("ctrl-shift-pageup", cx), 1);
        assert_eq!(active_after("ctrl-shift-pageup", cx), 0);
        assert_eq!(active_tabs(&ws, cx).0, [ids[1], ids[0], ids[2], ids[3]]);
        // At the start already: nothing moves.
        assert_eq!(active_after("ctrl-shift-pageup", cx), 0);
        assert_eq!(active_tabs(&ws, cx).0, [ids[1], ids[0], ids[2], ids[3]]);
    }

    #[gpui::test]
    fn the_system_prompt_opens_in_one_tab_that_is_kept(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, conn, cx) = connected(&agent, cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.new_session(conn.clone(), window, cx)
        });
        wait_until(cx, |cx| ws.read(cx).active_group.is_some());
        // The saved layout of the prompt tabs of the active session.
        let saved = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
            ws.read_with(cx, |ws, cx| {
                let group = ws.active_group().unwrap();
                group
                    .panes
                    .iter()
                    .flat_map(|pane| pane.tabs.iter())
                    .filter(|(_, panel)| matches!(panel, Panel::SystemPrompt(_)))
                    .filter_map(|(_, panel)| ws.saved_tab(group, panel, cx))
                    .collect::<Vec<_>>()
            })
        };

        cx.dispatch_action(ShowSystemPrompt);
        cx.run_until_parked();
        let session = ws.read_with(cx, |ws, _| {
            ws.active_group().unwrap().session.clone().unwrap().0
        });
        let prompt = SavedTab::SystemPrompt {
            session: session.clone(),
        };
        assert_eq!(saved(&ws, cx), std::slice::from_ref(&prompt));
        assert_eq!(active_tabs(&ws, cx).1, 1);
        wait_until(cx, |cx| {
            match ws.read(cx).active_group().and_then(|g| g.active_panel()) {
                Some(Panel::SystemPrompt(view)) => view.read(cx).is_loaded(),
                _ => false,
            }
        });

        // Showing it again uses the same tab.
        ws.update_in(cx, |ws, window, cx| {
            ws.activate_tab_in_active_pane(Some(0), window, cx)
        });
        cx.dispatch_action(ShowSystemPrompt);
        cx.run_until_parked();
        assert_eq!(saved(&ws, cx), [prompt]);
        assert_eq!(active_tabs(&ws, cx).1, 1);
    }

    #[gpui::test]
    fn tab_shortcuts_work_in_a_terminal(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, cx) = open_three_files(&agent, cx);
        ws.update_in(cx, |ws, window, cx| ws.new_terminal(None, window, cx));
        wait_until(cx, |cx| {
            matches!(
                ws.read(cx).active_group().and_then(|g| g.active_panel()),
                Some(Panel::Terminal(_))
            )
        });
        cx.run_until_parked();
        let terminal = active_tabs(&ws, cx).1;
        assert_eq!(terminal, 4);
        assert!(cx.update(|window, cx| {
            let ws = ws.read(cx);
            let Some(Panel::Terminal(view)) = ws.active_group().and_then(|g| g.active_panel())
            else {
                return false;
            };
            view.read(cx).focus_handle(cx).is_focused(window)
        }));

        cx.simulate_keystrokes("alt-1");
        cx.run_until_parked();
        assert_eq!(active_tabs(&ws, cx).1, 0);

        ws.update_in(cx, |ws, window, cx| {
            ws.activate_tab_in_active_pane(Some(terminal), window, cx)
        });
        cx.simulate_keystrokes("ctrl-tab");
        cx.run_until_parked();
        assert_eq!(active_tabs(&ws, cx).1, 0);

        ws.update_in(cx, |ws, window, cx| {
            ws.activate_tab_in_active_pane(Some(terminal), window, cx)
        });
        cx.simulate_keystrokes("ctrl-shift-pageup");
        cx.run_until_parked();
        assert_eq!(active_tabs(&ws, cx).1, terminal - 1);
    }

    /// The terminal tabs of the active pane, with where they are held.
    fn terminal_tabs(
        ws: &Entity<Workspace>,
        cx: &mut App,
    ) -> Vec<(Entity<TerminalView>, TerminalHolder)> {
        let pane = ws.read(cx).active_group().unwrap().active().unwrap();
        pane.tabs
            .iter()
            .filter_map(|(_, panel)| match panel {
                Panel::Terminal(view) => Some((view.clone(), view.read(cx).holder().clone())),
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    fn terminals_of_other_clients_get_tabs_and_can_be_taken_over(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, cx) = open_three_files(&agent, cx);
        let (phone, _phone_notes) = agent.client("loom-android");
        let terminal = agent.block_on({
            let phone = phone.clone();
            async move {
                let id = phone.open_terminal(80, 24, 7).await.unwrap();
                phone
                    .terminal_input(id, b"echo phone=$((3*3))\n".to_vec())
                    .await
                    .unwrap();
                id
            }
        });
        wait_until(cx, |cx| terminal_tabs(&ws, cx).len() == 1);
        let (view, holder) = cx.update(|_, cx| terminal_tabs(&ws, cx).remove(0));
        let TerminalHolder::Other(client) = holder else {
            panic!("held by {holder:?}");
        };
        assert_eq!(client.describe(), "the Android app");
        // The new tab does not take the focus.
        assert_eq!(active_tabs(&ws, cx).1, 3);
        let title = cx.update(|_, cx| Panel::Terminal(view.clone()).title(cx));
        assert_eq!(title, "Terminal (on the Android app)");

        // Showing its tab asks nothing; the button over it takes it over,
        // with its output.
        ws.update_in(cx, |ws, window, cx| {
            let pane = ws.active_group().unwrap().active_pane;
            ws.activate_tab(pane, 4, window, cx);
        });
        cx.run_until_parked();
        assert!(!has_dialog(cx));
        assert!(matches!(
            view.read_with(cx, |v, _| v.holder().clone()),
            TerminalHolder::Other(_)
        ));
        click("terminal-take-over", cx);
        wait_until(cx, |cx| {
            *view.read(cx).holder() == TerminalHolder::You
                && view.read(cx).snapshot().text().contains("phone=9")
        });
        let refused = agent.block_on({
            let phone = phone.clone();
            async move { phone.terminal_input(terminal, b"x".to_vec()).await }
        });
        assert!(refused.is_err());
        // Shells may take a paste as text to edit, so Enter runs it.
        view.update(cx, |view, cx| view.paste("echo desk=$((4*4))", cx));
        cx.simulate_keystrokes("enter");
        wait_until(cx, |cx| view.read(cx).snapshot().text().contains("desk=16"));

        // The phone takes it back, and the tab says so.
        agent.block_on({
            let phone = phone.clone();
            async move {
                phone
                    .attach_terminal(terminal, 80, 24, 7, true)
                    .await
                    .unwrap()
            }
        });
        wait_until(cx, |cx| {
            matches!(view.read(cx).holder(), TerminalHolder::Other(_))
        });

        // When the phone closes it, its tab goes.
        agent.block_on({
            let phone = phone.clone();
            async move { phone.close_terminal(terminal).await.unwrap() }
        });
        wait_until(cx, |cx| terminal_tabs(&ws, cx).is_empty());
    }

    #[gpui::test]
    fn closing_a_session_tab_archives_it_and_other_clients_terminals_stay(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, cx) = open_three_files(&agent, cx);
        let pane = ws.read_with(cx, |ws, _| ws.active_group().unwrap().active_pane);
        let close = |ix: usize, cx: &mut VisualTestContext| {
            ws.update_in(cx, |ws, window, cx| ws.close_tab(pane, ix, window, cx));
            cx.run_until_parked();
        };

        let kinds = |ws: &Entity<Workspace>, cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let pane = ws.read(cx).active_group().unwrap().active().unwrap();
                let tabs: Vec<_> = pane
                    .tabs
                    .iter()
                    .map(|(_, panel)| (panel.kind(), panel.closable(cx)))
                    .collect();
                tabs
            })
        };
        assert!(kinds(&ws, cx)[0] == (PanelKind::Chat, true));

        // A terminal of another client stays until it is taken over.
        let (phone, _phone_notes) = agent.client("loom-android");
        let terminal = agent.block_on({
            let phone = phone.clone();
            async move { phone.open_terminal(80, 24, 7).await.unwrap() }
        });
        wait_until(cx, |cx| terminal_tabs(&ws, cx).len() == 1);
        assert!(kinds(&ws, cx)[4] == (PanelKind::Terminal, false));
        close(4, cx);
        assert_eq!(cx.update(|_, cx| terminal_tabs(&ws, cx).len()), 1);
        let view = cx.update(|_, cx| terminal_tabs(&ws, cx).remove(0).0);
        ws.update(cx, |ws, cx| ws.take_over_terminal(&view, cx));
        wait_until(cx, |cx| *view.read(cx).holder() == TerminalHolder::You);
        assert!(kinds(&ws, cx)[4] == (PanelKind::Terminal, true));
        // Closing it here ends it, once its view is gone.
        drop(view);
        close(4, cx);
        assert!(cx.update(|_, cx| terminal_tabs(&ws, cx).is_empty()));
        wait_until(cx, |_| {
            let phone = phone.clone();
            agent
                .block_on(async move { phone.terminals().await.unwrap() })
                .iter()
                .all(|t| t.id != terminal)
        });

        // A new session gets a tab in the same group. Closing the tab of a
        // session archives it, by click or by ctrl-w, and another client's
        // terminal goes on.
        let other = agent.block_on({
            let phone = phone.clone();
            async move { phone.open_terminal(80, 24, 7).await.unwrap() }
        });
        wait_until(cx, |cx| terminal_tabs(&ws, cx).len() == 1);
        let (conn, first) = ws.read_with(cx, |ws, _| {
            let group = ws.active_group().unwrap();
            (group.connection.clone(), group.session.clone().unwrap())
        });
        ws.update_in(cx, |ws, window, cx| {
            ws.new_session(conn.clone(), window, cx)
        });
        wait_until(cx, |cx| ws.read(cx).groups[0].chats().count() == 2);
        assert_eq!(ws.read_with(cx, |ws, _| ws.groups.len()), 1);
        let ix = ws.read_with(cx, |ws, cx| {
            let (pane, ix) = ws.groups[0].find_chat(&first, cx).unwrap();
            assert_eq!(pane, ws.groups[0].active_pane);
            ix
        });
        close(ix, cx);
        wait_until(cx, |cx| {
            ws.read(cx).groups[0].find_chat(&first, cx).is_none()
        });
        assert_eq!(cx.update(|_, cx| terminal_tabs(&ws, cx).len()), 1);
        let sessions = conn.read_with(cx, |conn, _| conn.sessions.clone());
        assert!(sessions.iter().all(|s| s.id != first));
        let terminals = agent.block_on(async move { phone.terminals().await.unwrap() });
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].id, other);

        let chat = kinds(&ws, cx)
            .iter()
            .position(|kind| *kind == (PanelKind::Chat, true))
            .unwrap();
        ws.update_in(cx, |ws, window, cx| ws.activate_tab(pane, chat, window, cx));
        cx.dispatch_action(CloseTab);
        wait_until(cx, |cx| ws.read(cx).groups[0].chats().next().is_none());
    }

    #[gpui::test]
    fn panel_shortcuts_show_the_agents_notes_and_tasks_tabs(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        agent.write("a.txt", "one\n");
        let (ws, cx) = open_file(&agent, "a.txt", cx);
        let right =
            |cx: &mut VisualTestContext| ws.read_with(cx, |ws, _| (ws.show_right, ws.right_tab));
        cx.simulate_keystrokes("ctrl-shift-n");
        cx.run_until_parked();
        assert!(right(cx) == (true, RightTab::Notes));
        assert!(cx.update(|_, cx| notes_view(&ws, cx)).is_some());
        // The notes editor has the focus, and the workspace keys still work
        // from there.
        cx.simulate_keystrokes("ctrl-shift-t");
        cx.run_until_parked();
        assert!(right(cx) == (true, RightTab::Tasks));
        let tasks = cx
            .update(|_, cx| tasks_view(&ws, cx))
            .expect("no tasks view");
        let focused = cx.update(|window, cx| tasks.read(cx).input_focus(cx).is_focused(window));
        assert!(focused, "the new-task input has the focus");

        cx.simulate_keystrokes("ctrl-shift-b");
        cx.run_until_parked();
        assert!(right(cx) == (true, RightTab::Codespaces));
        // Showing is not toggling: a second press keeps the panel.
        cx.simulate_keystrokes("ctrl-shift-b");
        cx.run_until_parked();
        assert!(right(cx) == (true, RightTab::Codespaces));
    }

    #[gpui::test]
    fn host_colors_are_saved_and_go_with_their_host(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let (ws, conn, cx) = connected(&agent, cx);
        let entry = conn.read_with(cx, |conn, _| conn.host.clone());
        ws.update(cx, |ws, _| {
            ws.hosts.push(HostNode {
                entry,
                agents: Vec::new(),
                loading: false,
                error: None,
                oracle: None,
            })
        });
        let saved = |cx: &mut VisualTestContext| {
            let path = ws.read_with(cx, |ws, _| ws.config_path.clone());
            ClientConfig::load(&path).unwrap().appearance.host_colors
        };
        ws.update_in(cx, |ws, window, cx| {
            ws.set_host_color(0, Some("#3b82f6".into()), window, cx);
            assert_eq!(ws.host_color("local"), Some(gpui::rgb(0x3b82f6).into()));
            assert_eq!(ws.host_color("other"), None);
        });
        assert_eq!(saved(cx).get("local").map(String::as_str), Some("#3b82f6"));
        ws.update_in(cx, |ws, window, cx| {
            ws.set_host_color(0, None, window, cx);
            ws.set_host_color(0, Some("#22c55e".into()), window, cx);
            ws.remove_host(0, window, cx);
            assert_eq!(ws.host_color("local"), None);
        });
        assert!(saved(cx).is_empty());
    }

    /// The Changes panel lists the commits that the upstream branch does
    /// not have, and a committed diff opens in a tab of its own.
    #[gpui::test]
    fn committed_changes_open_in_their_own_diff_tabs(cx: &mut TestAppContext) {
        let agent = TestAgent::start();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(&agent.workspace)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        agent.write("a.txt", "one\n");
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        git(&["branch", "upstream"]);
        git(&["branch", "--set-upstream-to=upstream"]);
        agent.write("a.txt", "one\ntwo\n");
        git(&["commit", "-q", "-am", "Add two"]);
        agent.write("a.txt", "one\ntwo\nthree\n");

        let (ws, conn, cx) = connected(&agent, cx);
        ws.update_in(cx, |ws, window, cx| {
            ws.new_session(conn.clone(), window, cx)
        });
        wait_until(cx, |cx| ws.read(cx).active_group.is_some());
        conn.update(cx, |conn, cx| conn.refresh_changes(cx));
        wait_until(cx, |cx| conn.read(cx).git.changes.is_some());
        let changes = conn.read_with(cx, |conn, _| conn.git.changes.clone().unwrap());
        assert_eq!(changes.files.len(), 1);
        let committed = changes.committed.clone().unwrap();
        assert_eq!(committed.base, "upstream");
        assert_eq!(committed.commits[0].summary, "Add two");
        let change = committed.files[0].clone();
        assert_eq!(change.path, "a.txt");

        let diffs = |cx: &mut VisualTestContext| {
            ws.read_with(cx, |ws, cx| {
                ws.active_group()
                    .unwrap()
                    .panes
                    .iter()
                    .flat_map(|pane| pane.tabs.iter())
                    .filter_map(|(_, panel)| match panel {
                        Panel::Diff(view) => Some(view.clone()),
                        _ => None,
                    })
                    .map(|view| (view.read(cx).kind(), view.read(cx).title()))
                    .collect::<Vec<_>>()
            })
        };
        for committed in [true, false, true] {
            let (conn, change) = (conn.clone(), change.clone());
            ws.update_in(cx, |ws, window, cx| {
                ws.open_git_diff(conn, change, committed, window, cx)
            });
            cx.run_until_parked();
        }
        assert_eq!(
            diffs(cx),
            [
                (DiffKind::Committed, "a.txt (committed)".to_string()),
                (DiffKind::Uncommitted, "a.txt".to_string()),
            ]
        );
        let view = ws.read_with(cx, |ws, _| {
            match ws.active_group().unwrap().active_panel() {
                Some(Panel::Diff(view)) => view.clone(),
                _ => panic!("the committed diff is not active"),
            }
        });
        assert_eq!(
            view.read_with(cx, |view, _| view.kind()),
            DiffKind::Committed
        );
        wait_until(cx, |cx| view.read(cx).counts() == (1, 0));
        let saved = view.read_with(cx, |view, cx| view.saved_tab(cx));
        assert!(matches!(
            saved,
            SavedTab::GitDiff {
                committed: true,
                ..
            }
        ));

        // The panel draws both lists.
        ws.update(cx, |ws, cx| {
            ws.show_right = true;
            ws.right_tab = RightTab::Changes;
            cx.notify();
        });
        cx.run_until_parked();
    }
}
