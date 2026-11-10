use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape as AnsiCursorShape, NamedColor, Processor, Rgb,
};

use crate::{Cell, CellFlags, Color, Cursor, CursorShape, Modes, Snapshot, TerminalEmulator};

const SCROLLBACK: usize = 10_000;

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
            }
            lines[row as usize][col] = Cell {
                text,
                fg: self.color(cell.fg, colors),
                bg: self.color(cell.bg, colors),
                flags: cell_flags(cell.flags),
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let mut term = AlacrittyTerminal::new(10, 2);
        term.feed("\u{1112}\u{1161}\u{11ab}".as_bytes());
        let snap = term.snapshot();
        assert!(snap.line_text(0).starts_with('\u{1112}'));
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
}
