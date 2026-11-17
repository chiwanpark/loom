//! Terminals on an agent's host. The agent runs the shell; the core keeps
//! each terminal's screen with the desktop's emulator and hands the app
//! frames to draw, and it turns keys into the bytes programs expect. A
//! terminal is attached to one client at a time: another client (as the
//! desktop app) can take one over, and the app can take it back, or take
//! one that another client opened, with its recent output.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use loom_client::ClientError;
use loom_client::proto::{Command, ErrorCode, TerminalHolder, TerminalId, TerminalInfo};
use loom_vt::input::{Key, Modifiers, encode_key, encode_paste};
use loom_vt::{AlacrittyTerminal, CursorShape, TerminalEmulator};
use tokio::sync::{Notify, mpsc};

use crate::agent::{AgentConnection, Shared};
use crate::error::{LoomError, Result};
use crate::runtime::{run, spawn};

/// The most output kept for a terminal before the app knows of it, as when
/// its first output arrives before the agent's answer that opened it.
const MAX_EARLY_OUTPUT: usize = 64 * 1024;

/// Where a terminal of the agent is open.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum TerminalPlace {
    /// In this app.
    Here,
    /// In another client, which `client` names for people.
    Elsewhere { client: String },
    /// Nowhere: its client went away, and the agent closes it after a
    /// while unless a client takes it.
    Nowhere,
}

/// One of the agent's open terminals.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TerminalSummary {
    /// The terminal's number on the agent.
    pub id: u64,
    pub place: TerminalPlace,
}

/// What the app shows when another client closed a terminal it had taken.
const CLOSED_ELSEWHERE: &str = "The terminal was closed on another client.";

/// A color of a terminal's text or background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TerminalColor {
    /// The theme's text color.
    Foreground,
    /// The theme's background color.
    Background,
    /// One of the 16 colors that themes set: 0–7 normal, 8–15 bright.
    Palette { index: u8 },
    /// A color the program picked, as `0xRRGGBB`.
    Rgb { rgb: u32 },
}

/// Cells of a line in one style, from `column` on.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TerminalRun {
    pub column: u16,
    /// How many cells the text takes: two for a wide character, as in CJK.
    pub cells: u16,
    pub text: String,
    pub fg: TerminalColor,
    pub bg: TerminalColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    pub dim: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TerminalCursorShape {
    Block,
    Underline,
    Beam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TerminalCursor {
    pub row: u16,
    pub column: u16,
    pub shape: TerminalCursorShape,
}

/// What a terminal shows now.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TerminalFrame {
    pub cols: u16,
    pub rows: u16,
    /// The runs of each row, top first; cells in no run are blank.
    pub lines: Vec<Vec<TerminalRun>>,
    pub cursor: Option<TerminalCursor>,
    /// How many lines the view is scrolled back into the history.
    pub scrolled: u32,
    /// How many lines of history there are.
    pub history: u32,
    /// The title the program set, if any.
    pub title: Option<String>,
    /// The shell's exit status once it exited.
    pub exit_status: Option<i32>,
    /// Why the terminal ended otherwise, as when the agent closed it while
    /// the app was away.
    pub ended: Option<String>,
}

/// A key that does not stand for text, or a character typed with Ctrl or Alt.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum TerminalKey {
    Char { text: String },
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function { number: u8 },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// What goes to the agent for a terminal, in order.
enum Op {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
}

struct TermState {
    emulator: AlacrittyTerminal,
    changed: bool,
    exit_status: Option<i32>,
    ended: Option<String>,
    closed: bool,
    /// Whether the terminal is attached to this app, so it takes input.
    held: bool,
    /// Set while the app attaches it, so lists the agent sent before do
    /// not count.
    attaching: bool,
    /// Whether a list of the agent has shown the terminal. One that is
    /// missing from a list before that is new, not closed: the agent may
    /// have made the list before the terminal opened.
    seen: bool,
}

/// A terminal as the connection knows it, to feed it the agent's output.
pub(crate) struct TermShared {
    id: TerminalId,
    state: Mutex<TermState>,
    wake: Notify,
}

impl TermShared {
    fn new(id: TerminalId, cols: u16, rows: u16) -> Self {
        Self {
            id,
            state: Mutex::new(TermState {
                emulator: AlacrittyTerminal::new(cols, rows),
                changed: true,
                exit_status: None,
                ended: None,
                closed: false,
                held: true,
                attaching: false,
                seen: false,
            }),
            wake: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TermState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn id(&self) -> TerminalId {
        self.id
    }

    /// Takes in the shell's output; returns what the emulator answers, as
    /// to a query of the cursor's position, for the agent.
    pub(crate) fn output(&self, data: &[u8]) -> Vec<u8> {
        let mut state = self.lock();
        state.emulator.feed(data);
        state.changed = true;
        let responses = state.emulator.take_responses();
        drop(state);
        self.wake.notify_one();
        responses
    }

    pub(crate) fn exited(&self, status: i32) {
        let mut state = self.lock();
        state.exit_status = Some(status);
        state.changed = true;
        drop(state);
        self.wake.notify_one();
    }

    /// Ends the terminal for a reason other than its shell exiting.
    pub(crate) fn end(&self, why: &str) {
        let mut state = self.lock();
        if state.exit_status.is_none() && state.ended.is_none() {
            state.ended = Some(why.to_owned());
            state.changed = true;
        }
        drop(state);
        self.wake.notify_one();
    }

    pub(crate) fn is_live(&self) -> bool {
        let state = self.lock();
        state.exit_status.is_none() && state.ended.is_none() && !state.closed
    }

    /// Whether the terminal is attached here, or being attached.
    pub(crate) fn is_held(&self) -> bool {
        let state = self.lock();
        state.held || state.attaching
    }

    /// Clears the screen before the terminal is attached here, which
    /// replays its recent output. Returns whether it was held here.
    fn start_attach(&self) -> bool {
        let mut state = self.lock();
        let (cols, rows) = state.emulator.size();
        state.emulator = AlacrittyTerminal::new(cols, rows);
        state.attaching = true;
        state.changed = true;
        let held = state.held;
        drop(state);
        self.wake.notify_one();
        held
    }

    fn finish_attach(&self, held: bool) {
        let mut state = self.lock();
        state.attaching = false;
        state.held = held;
        state.changed = true;
        drop(state);
        self.wake.notify_one();
    }

    pub(crate) fn size(&self) -> (u16, u16) {
        self.lock().emulator.size()
    }
}

/// The terminals of a connection, output that came before its terminal,
/// and the agent's list of every terminal.
#[derive(Default)]
pub(crate) struct Terminals {
    pub(crate) open: HashMap<TerminalId, Arc<TermShared>>,
    early: HashMap<TerminalId, Vec<u8>>,
    /// The agent's terminals, as it last listed them.
    listed: Vec<TerminalInfo>,
    /// Whether the app should get the list again.
    pub(crate) changed: bool,
}

impl Terminals {
    /// Follows a terminal; returns the output that came for it before.
    pub(crate) fn add(&mut self, terminal: Arc<TermShared>) -> Vec<u8> {
        let early = self.early.remove(&terminal.id).unwrap_or_default();
        self.open.insert(terminal.id, terminal);
        early
    }

    /// Takes in output for a terminal; returns the terminal and what its
    /// emulator answers.
    pub(crate) fn output(&mut self, id: TerminalId, data: &[u8]) -> Option<(TerminalId, Vec<u8>)> {
        match self.open.get(&id) {
            Some(terminal) => Some((id, terminal.output(data))),
            None => {
                let early = self.early.entry(id).or_default();
                if early.len() + data.len() <= MAX_EARLY_OUTPUT {
                    early.extend_from_slice(data);
                }
                None
            }
        }
    }

    pub(crate) fn exited(&mut self, id: TerminalId, status: i32) {
        if let Some(terminal) = self.open.remove(&id) {
            terminal.exited(status);
        }
        self.early.remove(&id);
    }

    /// Ends every terminal, as when the connection closes for good.
    pub(crate) fn end_all(&mut self, why: &str) {
        for (_, terminal) in self.open.drain() {
            terminal.end(why);
        }
        self.early.clear();
    }

    /// Takes in the agent's list: notes which terminals are held here, and
    /// ends those that closed on another client after it took them.
    /// The agent sends only its latest list, so the list that shows another
    /// client with the terminal may never come: a terminal held here that
    /// a list showed before and that is now gone closed elsewhere too.
    pub(crate) fn listed(&mut self, terminals: Vec<TerminalInfo>) {
        let mut closed = Vec::new();
        for (id, terminal) in &self.open {
            let mut state = terminal.lock();
            if state.attaching {
                continue;
            }
            let info = terminals.iter().find(|t| t.id == *id);
            if info.is_some() {
                state.seen = true;
            }
            match info {
                // Held by no one: this app's earlier link, until the new
                // one attaches it again.
                Some(info) if info.holder == TerminalHolder::Nobody => {}
                Some(info) => state.held = info.holder == TerminalHolder::You,
                // One held here and not listed yet is new. One that exited
                // is gone already: the agent says so before it lists the rest.
                None if state.held && !state.seen => {}
                None => closed.push(*id),
            }
        }
        for id in closed {
            if let Some(terminal) = self.open.remove(&id) {
                terminal.end(CLOSED_ELSEWHERE);
            }
        }
        self.listed = terminals;
        self.changed = true;
    }

    /// The agent's terminals for the app; one the app attaches counts as
    /// here already.
    pub(crate) fn summaries(&self) -> Vec<TerminalSummary> {
        self.listed
            .iter()
            .map(|t| {
                let (attaching, held) = self.open.get(&t.id).map_or((false, false), |t| {
                    let state = t.lock();
                    let live =
                        state.exit_status.is_none() && state.ended.is_none() && !state.closed;
                    (state.attaching, state.held && live)
                });
                let place = match &t.holder {
                    _ if attaching => TerminalPlace::Here,
                    TerminalHolder::You => TerminalPlace::Here,
                    // This app's earlier link held it; the new one takes it back.
                    TerminalHolder::Nobody if held => TerminalPlace::Here,
                    TerminalHolder::Other(client) => TerminalPlace::Elsewhere {
                        client: client.describe(),
                    },
                    TerminalHolder::Nobody => TerminalPlace::Nowhere,
                };
                TerminalSummary { id: t.id, place }
            })
            .collect()
    }
}

/// The color the emulator gives, in the form the app draws.
fn color(color: loom_vt::Color) -> TerminalColor {
    match color {
        loom_vt::Color::DefaultForeground => TerminalColor::Foreground,
        loom_vt::Color::DefaultBackground => TerminalColor::Background,
        loom_vt::Color::Indexed(index @ 0..16) => TerminalColor::Palette { index },
        loom_vt::Color::Indexed(index) => TerminalColor::Rgb {
            rgb: xterm_color(index),
        },
        loom_vt::Color::Rgb(r, g, b) => TerminalColor::Rgb {
            rgb: u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b),
        },
    }
}

/// The color of an index past the 16 that themes set: xterm's 6×6×6 cube,
/// then 24 grays.
fn xterm_color(index: u8) -> u32 {
    let rgb = |r: u32, g: u32, b: u32| r << 16 | g << 8 | b;
    if index >= 232 {
        let level = 8 + 10 * u32::from(index - 232);
        return rgb(level, level, level);
    }
    let cube = u32::from(index - 16);
    let level = |n: u32| if n == 0 { 0 } else { 55 + 40 * n };
    rgb(level(cube / 36), level(cube / 6 % 6), level(cube % 6))
}

/// The runs of a row of cells. A run holds cells of one style with plain
/// ASCII text; any other character gets a run of its own, so the app puts
/// it at its cell even when its font draws it wider or narrower.
fn runs(cells: &[loom_vt::Cell]) -> Vec<TerminalRun> {
    let mut runs: Vec<TerminalRun> = Vec::new();
    // Trailing blanks in the background color need no run.
    let end = cells
        .iter()
        .rposition(|cell| {
            !(cell.text.trim().is_empty()
                && cell.bg == loom_vt::Color::DefaultBackground
                && !cell.flags.inverse
                && !cell.flags.underline)
        })
        .map_or(0, |last| last + 1);
    let mut joinable = false;
    for (column, cell) in cells[..end].iter().enumerate() {
        if cell.flags.wide_spacer {
            continue;
        }
        let flags = cell.flags;
        let (mut fg, mut bg) = (color(cell.fg), color(cell.bg));
        if flags.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.hidden {
            fg = bg;
        }
        let text = if cell.text.is_empty() {
            " "
        } else {
            &cell.text
        };
        let plain = !flags.wide && text.len() == 1 && text.is_ascii();
        let cells = if flags.wide { 2 } else { 1 };
        if let Some(last) = runs.last_mut()
            && joinable
            && plain
            && last.column + last.cells == column as u16
            && last.fg == fg
            && last.bg == bg
            && last.bold == flags.bold
            && last.italic == flags.italic
            && last.underline == flags.underline
            && last.strikeout == flags.strikeout
            && last.dim == flags.dim
        {
            last.text.push_str(text);
            last.cells += 1;
            continue;
        }
        runs.push(TerminalRun {
            column: column as u16,
            cells,
            text: text.to_owned(),
            fg,
            bg,
            bold: flags.bold,
            italic: flags.italic,
            underline: flags.underline,
            strikeout: flags.strikeout,
            dim: flags.dim,
        });
        joinable = plain;
    }
    runs
}

fn frame(state: &TermState) -> TerminalFrame {
    let snapshot = state.emulator.snapshot();
    TerminalFrame {
        cols: snapshot.cols as u16,
        rows: snapshot.rows as u16,
        lines: snapshot.lines.iter().map(|line| runs(line)).collect(),
        cursor: snapshot.cursor.and_then(|cursor| {
            let shape = match cursor.shape {
                CursorShape::Block => TerminalCursorShape::Block,
                CursorShape::Underline => TerminalCursorShape::Underline,
                CursorShape::Beam => TerminalCursorShape::Beam,
                CursorShape::Hidden => return None,
            };
            Some(TerminalCursor {
                row: cursor.row as u16,
                column: cursor.col as u16,
                shape,
            })
        }),
        scrolled: snapshot.display_offset as u32,
        history: snapshot.history as u32,
        title: state.emulator.title(),
        exit_status: state.exit_status,
        ended: state.ended.clone(),
    }
}

fn vt_key(key: TerminalKey) -> Option<Key> {
    Some(match key {
        TerminalKey::Char { text } => {
            let mut chars = text.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Key::Char(c)
        }
        TerminalKey::Enter => Key::Enter,
        TerminalKey::Tab => Key::Tab,
        TerminalKey::Backspace => Key::Backspace,
        TerminalKey::Escape => Key::Escape,
        TerminalKey::Up => Key::Up,
        TerminalKey::Down => Key::Down,
        TerminalKey::Left => Key::Left,
        TerminalKey::Right => Key::Right,
        TerminalKey::Home => Key::Home,
        TerminalKey::End => Key::End,
        TerminalKey::PageUp => Key::PageUp,
        TerminalKey::PageDown => Key::PageDown,
        TerminalKey::Insert => Key::Insert,
        TerminalKey::Delete => Key::Delete,
        TerminalKey::Function { number } => Key::F(number),
    })
}

/// A terminal on the agent's host. Input and resizes go to the agent in the
/// order they are made; the screen comes from [`Terminal::next_frame`].
#[derive(uniffi::Object)]
pub struct Terminal {
    connection: Weak<Shared>,
    term: Arc<TermShared>,
    ops: mpsc::UnboundedSender<Op>,
}

impl Terminal {
    /// A handle for the app on a terminal the connection follows.
    fn new(shared: &Arc<Shared>, term: Arc<TermShared>) -> Self {
        let (ops, receiver) = mpsc::unbounded_channel();
        let connection = Arc::downgrade(shared);
        spawn(write_ops(connection.clone(), term.id, receiver));
        Self {
            connection,
            term,
            ops,
        }
    }

    fn send(&self, op: Op) {
        // Gone once the terminal is closed; then there is nothing to send.
        let _ = self.ops.send(op);
    }

    fn input(&self, bytes: Vec<u8>) {
        if bytes.is_empty() || !self.term.is_live() {
            return;
        }
        let mut state = self.term.lock();
        // Another client holds it; the agent would refuse the input.
        if !state.held {
            return;
        }
        if state.emulator.snapshot().display_offset > 0 {
            state.emulator.scroll_to_bottom();
            state.changed = true;
            drop(state);
            self.term.wake.notify_one();
        }
        self.send(Op::Input(bytes));
    }
}

/// Sends a terminal's input, resizes, and close to the agent in order,
/// joining input that piled up meanwhile into one message.
async fn write_ops(connection: Weak<Shared>, id: TerminalId, mut ops: mpsc::UnboundedReceiver<Op>) {
    let mut next = None;
    loop {
        let op = match next.take() {
            Some(op) => op,
            None => match ops.recv().await {
                Some(op) => op,
                None => return,
            },
        };
        let Some(shared) = connection.upgrade() else {
            return;
        };
        let command = match op {
            Op::Input(mut data) => {
                loop {
                    match ops.try_recv() {
                        Ok(Op::Input(more)) => data.extend(more),
                        Ok(other) => {
                            next = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                Command::TerminalInput { terminal: id, data }
            }
            Op::Resize(cols, rows) => Command::ResizeTerminal {
                terminal: id,
                cols,
                rows,
            },
            Op::Close => Command::CloseTerminal { terminal: id },
        };
        let close = matches!(command, Command::CloseTerminal { .. });
        if let Ok(client) = shared.client() {
            // Sent in order without waiting for each answer, so typing does
            // not wait for the round trip; a refusal only shows in the log.
            match client.submit(command).await {
                Ok(answer) => {
                    spawn(async move {
                        if let Err(err) = answer.await {
                            tracing::debug!(error = %err, "the agent refused terminal input");
                        }
                    });
                }
                Err(err) => tracing::debug!(error = %err, "could not send terminal input"),
            }
        }
        if close {
            return;
        }
    }
}

#[uniffi::export]
impl Terminal {
    /// The terminal's number on the agent.
    pub fn id(&self) -> u64 {
        self.term.id
    }

    /// Waits for the screen to change and returns it, the first time at
    /// once; `None` once the terminal is closed. Call it in a loop.
    pub async fn next_frame(self: Arc<Self>) -> Option<TerminalFrame> {
        let term = self.term.clone();
        run(async move {
            loop {
                {
                    let mut state = term.lock();
                    if state.closed {
                        return None;
                    }
                    if std::mem::take(&mut state.changed) {
                        return Some(frame(&state));
                    }
                }
                term.wake.notified().await;
            }
        })
        .await
    }

    /// Types text, as from the keyboard.
    pub fn send_text(&self, text: String) {
        self.input(text.into_bytes());
    }

    /// Presses a key, with the modifiers held.
    pub fn send_key(&self, key: TerminalKey, modifiers: KeyModifiers) {
        let Some(key) = vt_key(key) else {
            return;
        };
        let modifiers = Modifiers {
            shift: modifiers.shift,
            alt: modifiers.alt,
            ctrl: modifiers.ctrl,
            meta: false,
        };
        let modes = self.term.lock().emulator.modes();
        if let Some(bytes) = encode_key(key, modifiers, modes) {
            self.input(bytes);
        }
    }

    /// Pastes text, marked as pasted for programs that ask for it.
    pub fn paste(&self, text: String) {
        let modes = self.term.lock().emulator.modes();
        self.input(encode_paste(&text, modes));
    }

    /// Fits the terminal to `cols` by `rows` cells. One that another client
    /// holds gets the size when it is taken over.
    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        let mut state = self.term.lock();
        if state.emulator.size() == (cols, rows) {
            return;
        }
        state.emulator.resize(cols, rows);
        state.changed = true;
        let held = state.held && !state.attaching;
        drop(state);
        self.term.wake.notify_one();
        if held {
            self.send(Op::Resize(cols, rows));
        }
    }

    /// Scrolls by `lines`, back into the history when positive. A program on
    /// the alternate screen, as a pager or an editor, gets them as mouse
    /// wheel or arrow keys when it asks for them.
    pub fn scroll(&self, lines: i32) {
        if lines == 0 {
            return;
        }
        let mut state = self.term.lock();
        let modes = state.emulator.modes();
        let (cols, rows) = state.emulator.size();
        let middle = loom_vt::GridPoint {
            line: i32::from(rows / 2),
            column: usize::from(cols / 2),
        };
        if let Some(reports) =
            loom_vt::mouse::scroll_report(middle, lines, Modifiers::default(), modes)
        {
            drop(state);
            self.input(reports.concat());
        } else if modes.alt_screen && modes.alternate_scroll {
            drop(state);
            self.input(loom_vt::mouse::alt_scroll(lines));
        } else {
            state.emulator.scroll(lines);
            state.changed = true;
            drop(state);
            self.term.wake.notify_one();
        }
    }

    /// The text on the screen, for copying.
    pub fn text(&self) -> String {
        self.term.lock().emulator.snapshot().text()
    }

    /// Whether the terminal is attached to this app, so it takes input.
    /// Another client may take it over; the agent's list then says where
    /// it is ([`crate::AgentUpdate::Terminals`]).
    pub fn is_here(&self) -> bool {
        self.term.is_held()
    }

    /// Takes the terminal back from the client that took it over. The
    /// screen starts over with the terminal's recent output.
    pub async fn take_over(self: Arc<Self>) -> Result<()> {
        let Some(shared) = self.connection.upgrade() else {
            return Err(LoomError::Closed);
        };
        let term = self.term.clone();
        run(async move { shared.attach_terminal(&term, true).await })
            .await
            .map(|_| ())
            .map_err(LoomError::from)
    }

    /// Closes the terminal, ending its shell; `next_frame` then returns
    /// `None`. (Not `close`, which Kotlin's `AutoCloseable` takes for
    /// freeing the object.)
    pub fn end(&self) {
        self.term.lock().closed = true;
        self.term.wake.notify_one();
        if let Some(shared) = self.connection.upgrade() {
            shared.remove_terminal(self.term.id);
        }
        self.send(Op::Close);
    }
}

#[uniffi::export]
impl AgentConnection {
    /// Opens a terminal of `cols` by `rows` cells running the agent's shell
    /// in its workspace. It stays open on the agent while the app is away
    /// for a while, and the connection takes it over again when it
    /// reconnects.
    pub async fn open_terminal(self: Arc<Self>, cols: u16, rows: u16) -> Result<Arc<Terminal>> {
        let shared = self.shared.clone();
        let (cols, rows) = (cols.max(2), rows.max(1));
        let client = shared.client()?;
        let owner = shared.terminal_owner;
        let id = run(async move { client.open_terminal(cols, rows, owner).await })
            .await
            .map_err(LoomError::from)?;
        let term = Arc::new(TermShared::new(id, cols, rows));
        let terminal = Terminal::new(&shared, term.clone());
        // What the shell printed before the terminal was known here.
        let early = shared.add_terminal(term.clone());
        if !early.is_empty() {
            let responses = term.output(&early);
            if !responses.is_empty() {
                terminal.send(Op::Input(responses));
            }
        }
        Ok(Arc::new(terminal))
    }

    /// Takes over a terminal that another client opened or holds, at
    /// `cols` by `rows` cells; its screen starts with the terminal's recent
    /// output.
    pub async fn attach_terminal(
        self: Arc<Self>,
        id: u64,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<Terminal>> {
        let shared = self.shared.clone();
        let term = Arc::new(TermShared::new(id, cols.max(2), rows.max(1)));
        let terminal = Terminal::new(&shared, term.clone());
        let attached = {
            let shared = shared.clone();
            let term = term.clone();
            run(async move { shared.attach_terminal(&term, true).await }).await
        };
        if let Err(err) = attached {
            shared.forget_terminal(&term);
            return Err(err.into());
        }
        Ok(Arc::new(terminal))
    }
}

/// How attaching a terminal went.
pub(crate) enum Attached {
    Here,
    /// Another client holds it, and it was not taken over.
    Elsewhere,
}

impl Shared {
    /// Attaches a terminal to this connection: the screen starts over, and
    /// the terminal's recent output comes first. Without `take_over`, one
    /// another client holds stays there.
    pub(crate) async fn attach_terminal(
        &self,
        term: &Arc<TermShared>,
        take_over: bool,
    ) -> std::result::Result<Attached, ClientError> {
        let was_held = term.start_attach();
        self.follow_terminal(term.clone());
        let (cols, rows) = term.size();
        let attached = match self.client() {
            Ok(client) => {
                client
                    .attach_terminal(term.id, cols, rows, self.terminal_owner, take_over)
                    .await
            }
            Err(_) => Err(ClientError::Closed),
        };
        let result = match attached {
            Ok(()) => Ok(Attached::Here),
            Err(ClientError::Api(err)) if err.code == ErrorCode::Conflict => {
                Ok(Attached::Elsewhere)
            }
            Err(err) => Err(err),
        };
        let held = match &result {
            Ok(Attached::Here) => true,
            Ok(Attached::Elsewhere) | Err(ClientError::Api(_)) => false,
            // The link dropped; the next one attaches it again if it was held.
            Err(_) => was_held,
        };
        term.finish_attach(held);
        self.terminals_changed();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(text: &str) -> TerminalFrame {
        let mut emulator = AlacrittyTerminal::new(20, 3);
        emulator.feed(text.as_bytes());
        frame(&TermState {
            emulator,
            changed: false,
            exit_status: None,
            ended: None,
            closed: false,
            held: true,
            attaching: false,
            seen: true,
        })
    }

    #[test]
    fn frames_join_plain_text_and_part_out_the_rest() {
        let frame = feed("ab\x1b[31mcd\x1b[0m 한x\r\n\x1b[7mi\x1b[0m");
        assert_eq!((frame.cols, frame.rows), (20, 3));
        let first: Vec<_> = frame.lines[0]
            .iter()
            .map(|run| (run.column, run.cells, run.text.as_str(), run.fg))
            .collect();
        assert_eq!(
            first,
            [
                (0, 2, "ab", TerminalColor::Foreground),
                (2, 2, "cd", TerminalColor::Palette { index: 1 }),
                (4, 1, " ", TerminalColor::Foreground),
                (5, 2, "한", TerminalColor::Foreground),
                (7, 1, "x", TerminalColor::Foreground),
            ]
        );
        // Inverse video swaps the theme's colors.
        let second = &frame.lines[1][0];
        assert_eq!(
            (second.fg, second.bg),
            (TerminalColor::Background, TerminalColor::Foreground)
        );
        assert!(frame.lines[2].is_empty());
        let cursor = frame.cursor.unwrap();
        assert_eq!((cursor.row, cursor.column), (1, 1));
    }

    #[test]
    fn colors_past_the_palette_are_xterms() {
        assert_eq!(xterm_color(16), 0x000000);
        assert_eq!(xterm_color(21), 0x0000ff);
        assert_eq!(xterm_color(196), 0xff0000);
        assert_eq!(xterm_color(232), 0x080808);
        assert_eq!(xterm_color(255), 0xeeeeee);
        let frame = feed("\x1b[38;5;196mr\x1b[48;2;1;2;3mg");
        assert_eq!(frame.lines[0][0].fg, TerminalColor::Rgb { rgb: 0xff0000 });
        assert_eq!(frame.lines[0][1].bg, TerminalColor::Rgb { rgb: 0x010203 });
    }

    #[test]
    fn a_held_terminal_that_leaves_the_list_ends_once_a_list_showed_it() {
        let mut terminals = Terminals::default();
        let term = Arc::new(TermShared::new(7, 20, 3));
        terminals.add(term.clone());
        // A list from before the terminal opened.
        terminals.listed(Vec::new());
        assert!(term.is_live());
        terminals.listed(vec![TerminalInfo {
            id: 7,
            holder: TerminalHolder::You,
        }]);
        assert!(term.is_live());
        // Another client took it and closed it, and the agent sent only
        // its latest list.
        terminals.listed(Vec::new());
        assert!(!term.is_live());
        assert_eq!(term.lock().ended.as_deref(), Some(CLOSED_ELSEWHERE));
        assert!(terminals.open.is_empty());
    }
}
