use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as AlacPoint, Side as AlacSide};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape as AnsiCursorShape, NamedColor, Processor, Rgb,
};

use crate::{
    Cell, CellFlags, ClipboardTarget, ClipboardWrite, Color, Cursor, CursorShape, GridPoint, Modes,
    SelectionKind, Side, Snapshot, TerminalEmulator,
};

const SCROLLBACK: usize = 10_000;
/// Clipboard writes larger than this are dropped instead of offered to the user.
pub const MAX_CLIPBOARD_WRITE: usize = 1 << 20;

#[derive(Clone, Copy)]
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Default)]
struct Shared {
    responses: Vec<u8>,
    title: Option<String>,
    clipboard: Option<ClipboardWrite>,
}

#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Shared>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut shared = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            Event::PtyWrite(text) => shared.responses.extend_from_slice(text.as_bytes()),
            Event::Title(title) => shared.title = Some(title),
            Event::ResetTitle => shared.title = None,
            // OSC 52 copy. Only the latest request is kept, and the GUI asks the user
            // before writing it, because terminal output is untrusted.
            Event::ClipboardStore(ty, text) if text.len() <= MAX_CLIPBOARD_WRITE => {
                let target = match ty {
                    ClipboardType::Clipboard => ClipboardTarget::Clipboard,
                    ClipboardType::Selection => ClipboardTarget::Primary,
                };
                shared.clipboard = Some(ClipboardWrite { target, text });
            }
            _ => {}
        }
    }
}

pub struct AlacrittyTerminal {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    size: Size,
}

impl AlacrittyTerminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = Size {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        };
        let listener = Listener::default();
        let config = Config {
            scrolling_history: SCROLLBACK,
            // OSC 52 reads are never answered; writes are queued for confirmation.
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, listener.clone()),
            parser: Processor::new(),
            listener,
            size,
        }
    }

    fn color(
        &self,
        color: AnsiColor,
        overrides: &alacritty_terminal::term::color::Colors,
    ) -> Color {
        let rgb = |rgb: Rgb| Color::Rgb(rgb.r, rgb.g, rgb.b);
        match color {
            AnsiColor::Spec(spec) => rgb(spec),
            AnsiColor::Indexed(index) => match overrides[usize::from(index)] {
                Some(spec) => rgb(spec),
                None => Color::Indexed(index),
            },
            AnsiColor::Named(named) => {
                if let Some(spec) = overrides[named as usize] {
                    return rgb(spec);
                }
                let index = named as usize;
                match named {
                    NamedColor::Foreground
                    | NamedColor::BrightForeground
                    | NamedColor::DimForeground
                    | NamedColor::Cursor => Color::DefaultForeground,
                    NamedColor::Background => Color::DefaultBackground,
                    _ if index < 16 => Color::Indexed(index as u8),
                    _ => {
                        let dim = index - NamedColor::DimBlack as usize;
                        Color::Indexed(dim.min(7) as u8)
                    }
                }
            }
        }
    }
}

fn alac_point(point: GridPoint) -> AlacPoint {
    AlacPoint::new(Line(point.line), Column(point.column))
}

fn alac_side(side: Side) -> AlacSide {
    match side {
        Side::Left => AlacSide::Left,
        Side::Right => AlacSide::Right,
    }
}

fn cell_flags(flags: Flags) -> CellFlags {
    CellFlags {
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
        strikeout: flags.contains(Flags::STRIKEOUT),
        inverse: flags.contains(Flags::INVERSE),
        dim: flags.contains(Flags::DIM),
        hidden: flags.contains(Flags::HIDDEN),
        wide: flags.contains(Flags::WIDE_CHAR),
        wide_spacer: flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
        selected: false,
    }
}

impl TerminalEmulator for AlacrittyTerminal {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.size = Size {
            cols: usize::from(cols.max(1)),
            rows: usize::from(rows.max(1)),
        };
        self.term.resize(self.size);
    }

    fn size(&self) -> (u16, u16) {
        (self.size.cols as u16, self.size.rows as u16)
    }

    fn snapshot(&self) -> Snapshot {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let colors = content.colors;
        let selection = content.selection;
        let mut lines = vec![vec![Cell::default(); self.size.cols]; self.size.rows];
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= self.size.rows || col >= self.size.cols {
                continue;
            }
            let cell = indexed.cell;
            let mut text = String::new();
            text.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                text.extend(extra.iter());
                // Decomposed Hangul arrives as a wide leading jamo with the
                // rest of the syllable as zero-width characters.
                if let Some(composed) = crate::hangul::compose(&text) {
                    text = composed;
                }
            }
            let mut flags = cell_flags(cell.flags);
            flags.selected = selection.is_some_and(|range| range.contains(indexed.point));
            lines[row as usize][col] = Cell {
                text,
                fg: self.color(cell.fg, colors),
                bg: self.color(cell.bg, colors),
                flags,
            };
        }
        let cursor_point = content.cursor.point;
        let cursor_row = cursor_point.line.0 + offset;
        let visible = content.mode.contains(TermMode::SHOW_CURSOR)
            && cursor_row >= 0
            && (cursor_row as usize) < self.size.rows;
        let shape = match content.cursor.shape {
            AnsiCursorShape::Block | AnsiCursorShape::HollowBlock => CursorShape::Block,
            AnsiCursorShape::Underline => CursorShape::Underline,
            AnsiCursorShape::Beam => CursorShape::Beam,
            AnsiCursorShape::Hidden => CursorShape::Hidden,
        };
        Snapshot {
            cols: self.size.cols,
            rows: self.size.rows,
            lines,
            cursor: visible.then_some(Cursor {
                row: cursor_row as usize,
                col: cursor_point.column.0,
                shape,
            }),
            display_offset: content.display_offset,
            history: self.term.grid().history_size(),
        }
    }

    fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    fn modes(&self) -> Modes {
        let mode = *self.term.mode();
        Modes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
            focus_reporting: mode.contains(TermMode::FOCUS_IN_OUT),
            mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
            mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            utf8_mouse: mode.contains(TermMode::UTF8_MOUSE),
            alternate_scroll: mode.contains(TermMode::ALTERNATE_SCROLL),
        }
    }

    fn title(&self) -> Option<String> {
        self.listener
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .title
            .clone()
    }

    fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(
            &mut self
                .listener
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .responses,
        )
    }

    fn take_clipboard_write(&mut self) -> Option<ClipboardWrite> {
        self.listener
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clipboard
            .take()
    }

    fn hyperlink_at(&self, point: GridPoint) -> Option<String> {
        let grid = self.term.grid();
        let line = point.line;
        if line < -(grid.history_size() as i32)
            || line >= self.size.rows as i32
            || point.column >= self.size.cols
        {
            return None;
        }
        let mut cell = &grid[alac_point(point)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && point.column > 0 {
            cell = &grid[AlacPoint::new(Line(line), Column(point.column - 1))];
        }
        cell.hyperlink().map(|link| link.uri().to_owned())
    }

    fn start_selection(&mut self, kind: SelectionKind, point: GridPoint, side: Side) {
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Semantic => SelectionType::Semantic,
            SelectionKind::Lines => SelectionType::Lines,
        };
        self.term.selection = Some(Selection::new(ty, alac_point(point), alac_side(side)));
    }

    fn update_selection(&mut self, point: GridPoint, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(alac_point(point), alac_side(side));
        }
    }

    fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    fn has_selection(&self) -> bool {
        self.term
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_empty())
    }

    fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    fn select_all(&mut self) {
        let start = AlacPoint::new(self.term.topmost_line(), Column(0));
        let end = AlacPoint::new(self.term.bottommost_line(), self.term.last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, AlacSide::Left);
        selection.update(end, AlacSide::Right);
        self.term.selection = Some(selection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queues_osc52_writes_and_ignores_reads() {
        let mut term = AlacrittyTerminal::new(20, 5);
        // "hi" and then "there", base64 encoded; only the latest write is kept.
        term.feed(b"\x1b]52;c;aGk=\x07\x1b]52;c;dGhlcmU=\x07");
        assert_eq!(
            term.take_clipboard_write(),
            Some(ClipboardWrite {
                target: ClipboardTarget::Clipboard,
                text: "there".into()
            })
        );
        assert_eq!(term.take_clipboard_write(), None);
        term.feed(b"\x1b]52;p;aGk=\x1b\\");
        assert_eq!(
            term.take_clipboard_write().map(|w| w.target),
            Some(ClipboardTarget::Primary)
        );
        term.feed(b"\x1b]52;c;?\x07");
        assert_eq!(term.take_clipboard_write(), None);
        assert!(term.take_responses().is_empty());
    }

    #[test]
    fn finds_hyperlinks_by_cell() {
        let mut term = AlacrittyTerminal::new(20, 5);
        term.feed("a \x1b]8;;https://example.com/\x1b\\link\x1b]8;;\x1b\\ 한\x1b]8;;file:///tmp\x07글\x1b]8;;\x07".as_bytes());
        let at = |column| term.hyperlink_at(GridPoint { line: 0, column });
        assert_eq!(at(0), None);
        assert_eq!(at(2).as_deref(), Some("https://example.com/"));
        assert_eq!(at(5).as_deref(), Some("https://example.com/"));
        assert_eq!(at(6), None);
        assert_eq!(at(9).as_deref(), Some("file:///tmp"));
        assert_eq!(at(10).as_deref(), Some("file:///tmp"));
        assert_eq!(at(99), None);
        assert_eq!(term.hyperlink_at(GridPoint { line: 7, column: 0 }), None);
    }

    #[test]
    fn renders_text_colors_and_cursor() {
        let mut term = AlacrittyTerminal::new(20, 5);
        term.feed(b"plain \x1b[1;31mred\x1b[0m\r\n\x1b[38;2;1;2;3mrgb\x1b[0m");
        let snap = term.snapshot();
        assert_eq!(snap.line_text(0), "plain red");
        let red = &snap.lines[0][6];
        assert_eq!(red.text, "r");
        assert_eq!(red.fg, Color::Indexed(1));
        assert!(red.flags.bold);
        assert_eq!(snap.lines[1][0].fg, Color::Rgb(1, 2, 3));
        assert_eq!(snap.lines[0][0].fg, Color::DefaultForeground);
        assert_eq!(
            snap.cursor,
            Some(Cursor {
                row: 1,
                col: 3,
                shape: CursorShape::Block
            })
        );
        assert_eq!(snap.text(), "plain red\nrgb");
    }

    #[test]
    fn handles_wide_hangul_and_combining_marks() {
        let mut term = AlacrittyTerminal::new(10, 2);
        term.feed("한글a".as_bytes());
        let snap = term.snapshot();
        assert_eq!(snap.lines[0][0].text, "한");
        assert!(snap.lines[0][0].flags.wide);
        assert!(snap.lines[0][1].flags.wide_spacer);
        assert_eq!(snap.lines[0][2].text, "글");
        assert_eq!(snap.lines[0][4].text, "a");
        assert_eq!(snap.cursor.unwrap().col, 5);
        assert_eq!(snap.line_text(0), "한글a");

        // Decomposed Hangul (as in macOS file names) shows as syllables in
        // the same wide cells.
        let mut term = AlacrittyTerminal::new(10, 2);
        term.feed("\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}.txt".as_bytes());
        let snap = term.snapshot();
        assert_eq!(snap.lines[0][0].text, "한");
        assert!(snap.lines[0][0].flags.wide);
        assert!(snap.lines[0][1].flags.wide_spacer);
        assert_eq!(snap.lines[0][2].text, "글");
        assert_eq!(snap.line_text(0), "한글.txt");
        assert_eq!(snap.cursor.unwrap().col, 8);
        // Copying keeps the original bytes, so a pasted name still matches the file.
        term.select_all();
        assert_eq!(
            term.selection_text().unwrap().trim_end(),
            "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}.txt"
        );
    }

    #[test]
    fn tracks_modes_title_responses_and_scrollback() {
        let mut term = AlacrittyTerminal::new(10, 3);
        term.feed(b"\x1b[?1h\x1b[?2004h\x1b]0;hello\x07\x1b[6n");
        let modes = term.modes();
        assert!(modes.app_cursor && modes.bracketed_paste && !modes.alt_screen);
        assert_eq!(term.title().as_deref(), Some("hello"));
        assert_eq!(term.take_responses(), b"\x1b[1;1R");
        assert!(term.take_responses().is_empty());

        for i in 0..10 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        let snap = term.snapshot();
        assert_eq!(snap.line_text(1), "line9");
        assert!(snap.history > 0);
        term.scroll(2);
        let snap = term.snapshot();
        assert_eq!(snap.display_offset, 2);
        assert_eq!(snap.line_text(0), "line6");
        term.scroll_to_bottom();
        assert_eq!(term.snapshot().display_offset, 0);

        term.resize(20, 5);
        assert_eq!(term.size(), (20, 5));
        assert_eq!(term.snapshot().lines[0].len(), 20);
        term.feed(b"\x1b[?1049h");
        assert!(term.modes().alt_screen);
    }

    #[test]
    fn selects_copies_and_marks_cells() {
        let mut term = AlacrittyTerminal::new(20, 3);
        term.feed(b"hello world\r\nsecond line");
        assert!(!term.has_selection());
        let at = |line, column| GridPoint { line, column };
        term.start_selection(SelectionKind::Simple, at(0, 0), Side::Left);
        term.update_selection(at(0, 4), Side::Right);
        assert!(term.has_selection());
        assert_eq!(term.selection_text().as_deref(), Some("hello"));
        let snap = term.snapshot();
        assert!(snap.lines[0][4].flags.selected);
        assert!(!snap.lines[0][5].flags.selected);

        term.start_selection(SelectionKind::Semantic, at(0, 8), Side::Left);
        assert_eq!(term.selection_text().as_deref(), Some("world"));
        term.start_selection(SelectionKind::Lines, at(1, 3), Side::Left);
        assert_eq!(term.selection_text().as_deref(), Some("second line\n"));

        term.select_all();
        assert_eq!(
            term.selection_text().as_deref(),
            Some("hello world\nsecond line\n")
        );
        term.clear_selection();
        assert!(term.selection_text().is_none());
    }

    #[test]
    fn reports_mouse_modes() {
        let mut term = AlacrittyTerminal::new(20, 3);
        assert!(!term.modes().mouse_reporting);
        term.feed(b"\x1b[?1002h\x1b[?1006h");
        let modes = term.modes();
        assert!(modes.mouse_reporting && modes.mouse_drag && modes.sgr_mouse);
        assert!(!modes.mouse_motion);
    }
}
