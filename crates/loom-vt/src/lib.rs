mod alacritty;
pub mod hangul;
pub mod input;
pub mod mouse;

pub use alacritty::{AlacrittyTerminal, MAX_CLIPBOARD_WRITE};

/// Which selection an OSC 52 write targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardTarget {
    Clipboard,
    Primary,
}

/// A clipboard write requested by the program in the terminal (OSC 52).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardWrite {
    pub target: ClipboardTarget,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    DefaultForeground,
    DefaultBackground,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CellFlags {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    pub inverse: bool,
    pub dim: bool,
    pub hidden: bool,
    pub wide: bool,
    pub wide_spacer: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: " ".into(),
            fg: Color::DefaultForeground,
            bg: Color::DefaultBackground,
            flags: CellFlags::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub alt_screen: bool,
    pub mouse_reporting: bool,
    pub focus_reporting: bool,
    pub mouse_motion: bool,
    pub mouse_drag: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub alternate_scroll: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPoint {
    pub line: i32,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Semantic,
    Lines,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<Vec<Cell>>,
    pub cursor: Option<Cursor>,
    pub display_offset: usize,
    pub history: usize,
}

impl Snapshot {
    pub fn line_text(&self, row: usize) -> String {
        self.lines
            .get(row)
            .map(|cells| {
                cells
                    .iter()
                    .filter(|c| !c.flags.wide_spacer)
                    .map(|c| c.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .unwrap_or_default()
    }

    pub fn text(&self) -> String {
        let mut lines: Vec<String> = (0..self.rows).map(|r| self.line_text(r)).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

pub trait TerminalEmulator: Send {
    fn feed(&mut self, bytes: &[u8]);
    fn resize(&mut self, cols: u16, rows: u16);
    fn size(&self) -> (u16, u16);
    fn snapshot(&self) -> Snapshot;
    fn scroll(&mut self, lines: i32);
    fn scroll_to_bottom(&mut self);
    fn modes(&self) -> Modes;
    fn title(&self) -> Option<String>;
    fn take_responses(&mut self) -> Vec<u8>;
    /// The latest pending OSC 52 clipboard write, if any. Nothing is written by the emulator.
    fn take_clipboard_write(&mut self) -> Option<ClipboardWrite>;
    /// The OSC 8 hyperlink at a grid point (lines below 0 are scrollback).
    fn hyperlink_at(&self, point: GridPoint) -> Option<String>;
    fn start_selection(&mut self, kind: SelectionKind, point: GridPoint, side: Side);
    fn update_selection(&mut self, point: GridPoint, side: Side);
    fn clear_selection(&mut self);
    fn has_selection(&self) -> bool;
    fn selection_text(&self) -> Option<String>;
    fn select_all(&mut self);
}
