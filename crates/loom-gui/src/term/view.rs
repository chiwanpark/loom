use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Hsla, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Pixels, Point, Render, Rgba, ScrollEvent, ShapedLine, SharedString,
    Size, Styled, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, canvas, div, fill,
    point, px, size,
};
use gpui_component::ActiveTheme;
use loom_vt::input::{encode_key, encode_paste, encode_text};
use loom_vt::{AlacrittyTerminal, CursorShape, Snapshot, TerminalEmulator};

use super::keys::map_key;
use super::palette::{Rgb, dim, resolve};

pub const MIN_COLS: u16 = 2;
pub const MIN_ROWS: u16 = 1;

pub trait TerminalIo: 'static {
    fn write(&self, data: Vec<u8>);
    fn resize(&self, cols: u16, rows: u16);
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Metrics {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
}

pub struct TerminalView {
    emulator: AlacrittyTerminal,
    io: Option<Rc<dyn TerminalIo>>,
    focus: FocusHandle,
    marked: Option<String>,
    metrics: Option<Metrics>,
    font: Font,
    font_size: Pixels,
    exited: Option<i32>,
    read_only: bool,
}

pub fn mono_font(family: Option<&str>, cx: &App) -> Font {
    let family: SharedString = family
        .map(|f| SharedString::from(f.to_owned()))
        .unwrap_or_else(|| cx.theme().mono_font_family.clone());
    gpui::font(family)
}

impl TerminalView {
    pub fn new(
        io: Option<Rc<dyn TerminalIo>>,
        font: Font,
        font_size: Pixels,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            emulator: AlacrittyTerminal::new(80, 24),
            io,
            focus: cx.focus_handle(),
            marked: None,
            metrics: None,
            font,
            font_size,
            exited: None,
            read_only: false,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn feed(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.emulator.feed(data);
        let responses = self.emulator.take_responses();
        if !responses.is_empty()
            && let Some(io) = &self.io
        {
            io.write(responses);
        }
        cx.notify();
    }

    pub fn set_exited(&mut self, status: i32, cx: &mut Context<Self>) {
        self.exited = Some(status);
        let message = format!("\r\n[process exited with status {status}]\r\n");
        self.emulator.feed(message.as_bytes());
        cx.notify();
    }

    pub fn exited(&self) -> Option<i32> {
        self.exited
    }

    pub fn title(&self) -> Option<String> {
        self.emulator.title()
    }

    pub fn grid_size(&self) -> (u16, u16) {
        self.emulator.size()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.emulator.snapshot()
    }

    pub fn marked_text(&self) -> Option<&str> {
        self.marked.as_deref()
    }

    fn writable(&self) -> Option<&Rc<dyn TerminalIo>> {
        if self.read_only || self.exited.is_some() {
            None
        } else {
            self.io.as_ref()
        }
    }

    fn send(&mut self, data: Vec<u8>, cx: &mut Context<Self>) {
        if data.is_empty() {
            return;
        }
        if let Some(io) = self.writable() {
            io.write(data);
            self.emulator.scroll_to_bottom();
            cx.notify();
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(MIN_COLS);
        let rows = rows.max(MIN_ROWS);
        if self.emulator.size() == (cols, rows) {
            return false;
        }
        self.emulator.resize(cols, rows);
        if let Some(io) = &self.io {
            io.resize(cols, rows);
        }
        true
    }

    pub fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let data = encode_paste(text, self.emulator.modes());
        self.send(data, cx);
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_some() {
            return;
        }
        let keystroke = &event.keystroke;
        let mods = keystroke.modifiers;
        if mods.control && mods.shift {
            match keystroke.key.as_str() {
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                        self.paste(&text, cx);
                    }
                    cx.stop_propagation();
                }
                "c" => {
                    let text = self.emulator.snapshot().text();
                    cx.write_to_clipboard(ClipboardItem::new_string(text.trim_end().to_owned()));
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }
        let Some((key, vt_mods)) = map_key(&keystroke.key, mods) else {
            return;
        };
        if let Some(bytes) = encode_key(key, vt_mods, self.emulator.modes()) {
            self.send(bytes, cx);
            cx.stop_propagation();
        }
        let _ = window;
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self
            .metrics
            .map(|m| m.cell.height)
            .unwrap_or(self.font_size);
        let delta = event.delta.pixel_delta(line_height).y;
        let lines = (delta / line_height).round() as i32;
        if lines != 0 {
            self.emulator.scroll(lines);
            cx.notify();
        }
    }

    fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let metrics = self.metrics?;
        let cursor = self.emulator.snapshot().cursor?;
        Some(Bounds::new(
            point(
                metrics.origin.x + metrics.cell.width * cursor.col as f32,
                metrics.origin.y + metrics.cell.height * cursor.row as f32,
            ),
            metrics.cell,
        ))
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked.as_deref().unwrap_or("");
        let units: Vec<u16> = marked.encode_utf16().collect();
        let end = range.end.min(units.len());
        let start = range.start.min(end);
        *adjusted_range = Some(start..end);
        Some(String::from_utf16_lossy(&units[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.marked.as_deref().map(utf16_len).unwrap_or(0);
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_deref().map(|m| 0..utf16_len(m))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        self.send(encode_text(text), cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds()
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        !self.read_only && self.exited.is_none()
    }
}

#[derive(Clone, Copy)]
struct Colors {
    foreground: Rgb,
    background: Rgb,
    cursor: Hsla,
    marked_background: Hsla,
}

fn to_rgb(color: Hsla) -> Rgb {
    let rgba: Rgba = color.into();
    (
        (rgba.r * 255.0).round() as u8,
        (rgba.g * 255.0).round() as u8,
        (rgba.b * 255.0).round() as u8,
    )
}

fn hsla(rgb: Rgb) -> Hsla {
    Rgba {
        r: rgb.0 as f32 / 255.0,
        g: rgb.1 as f32 / 255.0,
        b: rgb.2 as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

struct Segment {
    col: usize,
    offset: Pixels,
    line: ShapedLine,
}

struct RowLayout {
    backgrounds: Vec<(usize, usize, Hsla)>,
    segments: Vec<Segment>,
}

struct Layout {
    metrics: Metrics,
    rows: Vec<RowLayout>,
    cursor: Option<(usize, usize, CursorShape, Hsla)>,
    marked: Option<ShapedLine>,
    marked_background: Hsla,
}

struct Style {
    fg: Rgb,
    bg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn cell_style(cell: &loom_vt::Cell, colors: &Colors) -> Style {
    let flags = cell.flags;
    let mut fg = resolve(cell.fg, colors.foreground, flags.bold);
    let mut bg = resolve(cell.bg, colors.background, false);
    if flags.dim {
        fg = dim(fg);
    }
    if flags.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.hidden {
        fg = bg;
    }
    Style {
        fg,
        bg,
        bold: flags.bold,
        italic: flags.italic,
        underline: flags.underline,
        strike: flags.strikeout,
    }
}

fn text_run(len: usize, style: &Style, font: &Font) -> TextRun {
    let mut font = font.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    let color = hsla(style.fg);
    TextRun {
        len,
        font,
        color,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.0),
            color: Some(color),
            wavy: false,
        }),
        strikethrough: style.strike.then_some(gpui::StrikethroughStyle {
            thickness: px(1.0),
            color: Some(color),
        }),
    }
}

fn layout_rows(
    snapshot: &Snapshot,
    colors: &Colors,
    font: &Font,
    font_size: Pixels,
    cell_width: Pixels,
    window: &mut Window,
) -> Vec<RowLayout> {
    let text_system = window.text_system().clone();
    let default_bg = colors.background;
    snapshot
        .lines
        .iter()
        .map(|line| {
            let mut backgrounds: Vec<(usize, usize, Hsla)> = Vec::new();
            let mut segments = Vec::new();
            let mut text = String::new();
            let mut runs: Vec<TextRun> = Vec::new();
            let mut start_col = 0;
            let flush = |text: &mut String,
                         runs: &mut Vec<TextRun>,
                         start_col: usize,
                         force: bool,
                         segments: &mut Vec<Segment>| {
                if text.trim().is_empty() {
                    text.clear();
                    runs.clear();
                    return;
                }
                let shaped = text_system.shape_line(
                    SharedString::from(std::mem::take(text)),
                    font_size,
                    runs,
                    force.then_some(cell_width),
                );
                runs.clear();
                let offset = if force {
                    px(0.)
                } else {
                    ((cell_width * 2.0 - shaped.width()) / 2.0).max(px(0.))
                };
                segments.push(Segment {
                    col: start_col,
                    offset,
                    line: shaped,
                });
            };
            for (col, cell) in line.iter().enumerate() {
                if cell.flags.wide_spacer {
                    continue;
                }
                let style = cell_style(cell, colors);
                let width = if cell.flags.wide { 2 } else { 1 };
                if style.bg != default_bg {
                    let bg = hsla(style.bg);
                    match backgrounds.last_mut() {
                        Some((s, w, c)) if *c == bg && *s + *w == col => *w += width,
                        _ => backgrounds.push((col, width, bg)),
                    }
                }
                let content = if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                };
                if cell.flags.wide {
                    flush(&mut text, &mut runs, start_col, true, &mut segments);
                    let mut single = content.to_owned();
                    let run = vec![text_run(single.len(), &style, font)];
                    flush(&mut single, &mut run.clone(), col, false, &mut segments);
                    start_col = col + 2;
                    continue;
                }
                if text.is_empty() {
                    start_col = col;
                }
                text.push_str(content);
                let run = text_run(content.len(), &style, font);
                match runs.last_mut() {
                    Some(last)
                        if last.font == run.font
                            && last.color == run.color
                            && last.underline == run.underline
                            && last.strikethrough == run.strikethrough =>
                    {
                        last.len += run.len
                    }
                    _ => runs.push(run),
                }
            }
            flush(&mut text, &mut runs, start_col, true, &mut segments);
            RowLayout {
                backgrounds,
                segments,
            }
        })
        .collect()
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let theme = cx.theme();
        let colors = Colors {
            foreground: to_rgb(theme.foreground),
            background: to_rgb(theme.background),
            cursor: theme.caret,
            marked_background: theme.selection,
        };
        let font = self.font.clone();
        let font_size = self.font_size;
        let focus = self.focus.clone();
        let prepaint_view = view.clone();
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .bg(hsla(colors.background))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.focus.focus(window, cx);
                }),
            )
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let font_id = window.text_system().resolve_font(&font);
                        let cell_width = window
                            .text_system()
                            .advance(font_id, font_size, 'm')
                            .map(|s| s.width)
                            .unwrap_or(font_size * 0.6);
                        let line_height = (font_size * 1.3).round();
                        let cols = (bounds.size.width / cell_width).floor() as u16;
                        let rows = (bounds.size.height / line_height).floor() as u16;
                        let metrics = Metrics {
                            origin: bounds.origin,
                            cell: size(cell_width, line_height),
                        };
                        let (snapshot, marked, focused) = prepaint_view.update(cx, |view, _| {
                            view.resize(cols, rows);
                            view.metrics = Some(metrics);
                            (
                                view.emulator.snapshot(),
                                view.marked.clone(),
                                view.focus.is_focused(window),
                            )
                        });
                        let rows =
                            layout_rows(&snapshot, &colors, &font, font_size, cell_width, window);
                        let cursor = snapshot
                            .cursor
                            .filter(|_| snapshot.display_offset == 0)
                            .map(|c| {
                                let shape = if focused {
                                    c.shape
                                } else {
                                    CursorShape::Underline
                                };
                                (c.row, c.col, shape, colors.cursor)
                            });
                        let marked = marked.map(|text| {
                            let color = hsla(colors.foreground);
                            let run = TextRun {
                                len: text.len(),
                                font: font.clone(),
                                color,
                                background_color: None,
                                underline: Some(UnderlineStyle {
                                    thickness: px(1.0),
                                    color: Some(color),
                                    wavy: false,
                                }),
                                strikethrough: None,
                            };
                            window.text_system().shape_line(
                                SharedString::from(text),
                                font_size,
                                &[run],
                                None,
                            )
                        });
                        Layout {
                            metrics,
                            rows,
                            cursor,
                            marked,
                            marked_background: colors.marked_background,
                        }
                    },
                    move |bounds, layout, window, cx| {
                        let Metrics { origin, cell } = layout.metrics;
                        for (row, line) in layout.rows.iter().enumerate() {
                            let y = origin.y + cell.height * row as f32;
                            for (col, width, color) in &line.backgrounds {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(origin.x + cell.width * *col as f32, y),
                                        size(cell.width * *width as f32, cell.height),
                                    ),
                                    *color,
                                ));
                            }
                        }
                        if let Some((row, col, shape, color)) = layout.cursor {
                            let x = origin.x + cell.width * col as f32;
                            let y = origin.y + cell.height * row as f32;
                            let bounds = match shape {
                                CursorShape::Block => Some(Bounds::new(point(x, y), cell)),
                                CursorShape::Beam => {
                                    Some(Bounds::new(point(x, y), size(px(2.0), cell.height)))
                                }
                                CursorShape::Underline => Some(Bounds::new(
                                    point(x, y + cell.height - px(2.0)),
                                    size(cell.width, px(2.0)),
                                )),
                                CursorShape::Hidden => None,
                            };
                            if let Some(bounds) = bounds {
                                let color = if shape == CursorShape::Block {
                                    color.opacity(0.6)
                                } else {
                                    color
                                };
                                window.paint_quad(fill(bounds, color));
                            }
                        }
                        for (row, line) in layout.rows.iter().enumerate() {
                            let y = origin.y + cell.height * row as f32;
                            for segment in &line.segments {
                                let x = origin.x + cell.width * segment.col as f32 + segment.offset;
                                let _ = segment.line.paint(
                                    point(x, y),
                                    cell.height,
                                    TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        }
                        if let (Some(marked), Some((row, col, _, _))) =
                            (&layout.marked, layout.cursor)
                        {
                            let at = point(
                                origin.x + cell.width * col as f32,
                                origin.y + cell.height * row as f32,
                            );
                            window.paint_quad(fill(
                                Bounds::new(at, size(marked.width(), cell.height)),
                                layout.marked_background,
                            ));
                            let _ =
                                marked.paint(at, cell.height, TextAlign::Left, None, window, cx);
                        }
                        window.handle_input(
                            &focus,
                            ElementInputHandler::new(bounds, view.clone()),
                            cx,
                        );
                    },
                )
                .size_full(),
            )
    }
}
