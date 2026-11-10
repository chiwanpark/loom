use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction,
    ParentElement, Pixels, Point, Render, Rgba, ScrollEvent, ShapedLine, SharedString, Size,
    Styled, TextAlign, TextRun, TouchPhase, UTF16Selection, UnderlineStyle, Window, canvas, div,
    fill, point, px, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Sizable, h_flex};
use loom_vt::input::{Modifiers as VtModifiers, encode_key, encode_paste, encode_text};
use loom_vt::mouse::{self, CellMetrics, MouseButton as VtButton};
use loom_vt::{
    AlacrittyTerminal, ClipboardTarget, ClipboardWrite, CursorShape, GridPoint, SelectionKind,
    Side, Snapshot, TerminalEmulator,
};

use super::keys::map_key;
use super::palette::{Ansi, Rgb, ThemeColors, dim, resolve, theme_ansi};

pub const MIN_COLS: u16 = 2;
pub const MIN_ROWS: u16 = 1;
const DRAG_THRESHOLD: f32 = 3.0;
const CONTEXT: &str = "Terminal";
/// Characters of a requested clipboard write shown in the confirmation banner.
const CLIPBOARD_PREVIEW: usize = 80;
/// Link schemes that may be opened from terminal output, after confirmation.
const LINK_SCHEMES: &[&str] = &["http://", "https://", "mailto:"];

/// Whether a hyperlink from terminal output may be offered for opening.
pub fn openable_link(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    LINK_SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
        && !uri.chars().any(char::is_control)
}

/// One-line preview of clipboard text for the confirmation banner.
pub fn clipboard_preview(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut preview: String = flat.chars().take(CLIPBOARD_PREVIEW).collect();
    if flat.chars().count() > CLIPBOARD_PREVIEW {
        preview.push('…');
    }
    preview
}

/// How OSC 52 clipboard writes from the program are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardPolicy {
    Ask,
    Allow,
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NoAction, Some(CONTEXT)),
        KeyBinding::new("shift-tab", NoAction, Some(CONTEXT)),
        KeyBinding::new("ctrl-c", NoAction, Some(CONTEXT)),
    ]);
}

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
    display_offset: usize,
    mouse_down_at: Option<Point<Pixels>>,
    selecting: bool,
    reported_button: Option<VtButton>,
    scroll_px: Pixels,
    clipboard_policy: ClipboardPolicy,
    pending_clipboard: Option<ClipboardWrite>,
}

pub fn mono_font(family: Option<&str>, fallbacks: &[String], cx: &App) -> Font {
    let family: SharedString = family
        .map(|f| SharedString::from(f.to_owned()))
        .unwrap_or_else(|| cx.theme().mono_font_family.clone());
    let mut font = gpui::font(family);
    if !fallbacks.is_empty() {
        font.fallbacks = Some(gpui::FontFallbacks::from_fonts(fallbacks.to_vec()));
    }
    font
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
            display_offset: 0,
            mouse_down_at: None,
            selecting: false,
            reported_button: None,
            clipboard_policy: ClipboardPolicy::Ask,
            pending_clipboard: None,
            scroll_px: px(0.),
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
        if let Some(write) = self.emulator.take_clipboard_write()
            && !self.read_only
        {
            match self.clipboard_policy {
                ClipboardPolicy::Allow => write_clipboard(write, cx),
                ClipboardPolicy::Ask => self.pending_clipboard = Some(write),
            }
        }
        cx.notify();
    }

    fn resolve_clipboard(&mut self, allow: bool, always: bool, cx: &mut Context<Self>) {
        if let Some(write) = self.pending_clipboard.take()
            && allow
        {
            write_clipboard(write, cx);
        }
        if always {
            self.clipboard_policy = ClipboardPolicy::Allow;
        }
        cx.notify();
    }

    fn clipboard_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let write = self.pending_clipboard.as_ref()?;
        let theme = cx.theme();
        let target = match write.target {
            ClipboardTarget::Clipboard => "clipboard",
            ClipboardTarget::Primary => "primary selection",
        };
        let count = write.text.chars().count();
        Some(
            h_flex()
                .id("terminal-clipboard-request")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .gap_2()
                .px_2()
                .py_1()
                .bg(theme.popover)
                .border_b_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(theme.popover_foreground)
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(format!(
                            "The program wants to copy {count} characters to the {target}: {}",
                            clipboard_preview(&write.text)
                        )),
                )
                .child(
                    Button::new("terminal-clipboard-allow")
                        .small()
                        .primary()
                        .label("Allow")
                        .on_click(
                            cx.listener(|this, _, _, cx| this.resolve_clipboard(true, false, cx)),
                        ),
                )
                .child(
                    Button::new("terminal-clipboard-always")
                        .small()
                        .label("Always allow here")
                        .on_click(
                            cx.listener(|this, _, _, cx| this.resolve_clipboard(true, true, cx)),
                        ),
                )
                .child(
                    Button::new("terminal-clipboard-deny")
                        .small()
                        .label("Deny")
                        .on_click(
                            cx.listener(|this, _, _, cx| this.resolve_clipboard(false, false, cx)),
                        ),
                ),
        )
    }

    /// Ctrl+click on an OSC 8 hyperlink asks before opening it.
    fn open_link_at(&self, point: GridPoint, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(uri) = self.emulator.hyperlink_at(point) else {
            return false;
        };
        if !openable_link(&uri) {
            return false;
        }
        crate::ui::dialogs::confirm(
            "Open this link?",
            format!(
                "A program in the terminal links to\n{uri}\nOpen it only if you trust where it leads."
            ),
            "Open",
            move |_, cx| cx.open_url(&uri),
            window,
            cx,
        );
        true
    }

    pub fn set_exited(&mut self, status: i32, cx: &mut Context<Self>) {
        self.exited = Some(status);
        let message = format!("\r\n[process exited with status {status}]\r\n");
        self.emulator.feed(message.as_bytes());
        cx.notify();
    }

    /// Ends a terminal whose process can no longer be reached, for example because
    /// the agent restarted while the client was disconnected.
    pub fn set_lost(&mut self, reason: &str, cx: &mut Context<Self>) {
        self.exited = Some(-1);
        self.pending_clipboard = None;
        let message = format!("\r\n[{reason}]\r\n");
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
            self.emulator.clear_selection();
            cx.notify();
        }
    }

    fn write_raw(&self, data: Vec<u8>) {
        if !data.is_empty()
            && let Some(io) = self.writable()
        {
            io.write(data);
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.emulator.selection_text()
    }

    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let metrics = self.metrics?;
        let (cols, rows) = self.emulator.size();
        let cell = CellMetrics {
            cell_width: f32::from(metrics.cell.width),
            line_height: f32::from(metrics.cell.height),
            cols: usize::from(cols),
            rows: usize::from(rows),
        };
        let relative = position - metrics.origin;
        Some(mouse::point_and_side(
            f32::from(relative.x),
            f32::from(relative.y),
            cell,
            self.display_offset,
        ))
    }

    fn reports_mouse(&self, shift: bool) -> bool {
        self.writable().is_some() && mouse::wants_mouse(self.emulator.modes(), shift)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        if event.button == MouseButton::Left
            && event.modifiers.control
            && event.click_count == 1
            && self.open_link_at(point, window, cx)
        {
            return;
        }
        let button = vt_button(event.button);
        if self.reports_mouse(event.modifiers.shift) {
            if let Some(button) = button
                && let Some(bytes) = mouse::button_report(
                    point,
                    button,
                    vt_modifiers(event.modifiers),
                    true,
                    self.emulator.modes(),
                )
            {
                self.reported_button = Some(button);
                self.write_raw(bytes);
            }
            return;
        }
        match event.button {
            MouseButton::Left => {
                self.mouse_down_at = Some(event.position);
                self.selecting = false;
                let kind = match event.click_count {
                    0 => return,
                    1 => SelectionKind::Simple,
                    2 => SelectionKind::Semantic,
                    _ => SelectionKind::Lines,
                };
                if kind == SelectionKind::Simple
                    && event.modifiers.shift
                    && self.emulator.has_selection()
                {
                    self.emulator.update_selection(point, side);
                    self.selecting = true;
                } else {
                    self.emulator.start_selection(kind, point, side);
                    self.selecting = kind != SelectionKind::Simple;
                }
                cx.notify();
            }
            MouseButton::Middle => {
                if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                    self.paste(&text, cx);
                }
            }
            _ => {}
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        if self.reports_mouse(event.modifiers.shift) {
            let held = event.pressed_button.and_then(vt_button);
            if (held.is_none() || self.reported_button.is_some())
                && let Some(bytes) = mouse::motion_report(
                    point,
                    held,
                    vt_modifiers(event.modifiers),
                    self.emulator.modes(),
                )
            {
                self.write_raw(bytes);
            }
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(down) = self.mouse_down_at else {
            return;
        };
        if !self.selecting {
            let moved = event.position - down;
            let distance = (f32::from(moved.x).powi(2) + f32::from(moved.y).powi(2)).sqrt();
            if distance <= DRAG_THRESHOLD {
                return;
            }
            self.selecting = true;
        }
        self.emulator.update_selection(point, side);
        if !self.emulator.modes().alt_screen
            && let Some(metrics) = self.metrics
        {
            let (_, rows) = self.emulator.size();
            let top = metrics.origin.y;
            let bottom = top + metrics.cell.height * f32::from(rows);
            if event.position.y < top {
                self.emulator.scroll(1);
            } else if event.position.y > bottom {
                self.emulator.scroll(-1);
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(button) = self.reported_button.take() {
            if let Some((point, _)) = self.grid_point(event.position)
                && let Some(bytes) = mouse::button_report(
                    point,
                    button,
                    vt_modifiers(event.modifiers),
                    false,
                    self.emulator.modes(),
                )
            {
                self.write_raw(bytes);
            }
            return;
        }
        if event.button == MouseButton::Left {
            if self.selecting
                && let Some(text) = self.emulator.selection_text()
            {
                cx.write_to_primary(ClipboardItem::new_string(text));
            } else if !self.selecting && self.emulator.has_selection() {
                self.emulator.clear_selection();
            }
            self.mouse_down_at = None;
            self.selecting = false;
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
                    if let Some(text) = self.emulator.selection_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    cx.stop_propagation();
                }
                "a" => {
                    self.emulator.select_all();
                    cx.notify();
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
        if event.touch_phase == TouchPhase::Started {
            self.scroll_px = px(0.);
            return;
        }
        let before = (self.scroll_px / line_height) as i32;
        self.scroll_px += event.delta.pixel_delta(line_height).y;
        let lines = (self.scroll_px / line_height) as i32 - before;
        if let Some(metrics) = self.metrics {
            let (_, rows) = self.emulator.size();
            let height = metrics.cell.height * f32::from(rows.max(1));
            self.scroll_px %= height;
        }
        if lines == 0 {
            return;
        }
        let shift = event.modifiers.shift;
        let modes = self.emulator.modes();
        if self.reports_mouse(shift) {
            if let Some((point, _)) = self.grid_point(event.position)
                && let Some(reports) =
                    mouse::scroll_report(point, lines, vt_modifiers(event.modifiers), modes)
            {
                for report in reports {
                    self.write_raw(report);
                }
            }
        } else if self.writable().is_some() && modes.alt_screen && modes.alternate_scroll && !shift
        {
            self.write_raw(mouse::alt_scroll(lines));
        } else {
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

fn vt_button(button: MouseButton) -> Option<VtButton> {
    match button {
        MouseButton::Left => Some(VtButton::Left),
        MouseButton::Middle => Some(VtButton::Middle),
        MouseButton::Right => Some(VtButton::Right),
        _ => None,
    }
}

fn vt_modifiers(mods: gpui::Modifiers) -> VtModifiers {
    VtModifiers {
        shift: mods.shift,
        alt: mods.alt,
        ctrl: mods.control,
        meta: mods.platform,
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
    selection: Rgb,
    ansi: Ansi,
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
    let mut fg = resolve(cell.fg, colors.foreground, flags.bold, &colors.ansi);
    let mut bg = resolve(cell.bg, colors.background, false, &colors.ansi);
    if flags.dim {
        fg = dim(fg);
    }
    if flags.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.hidden {
        fg = bg;
    }
    if flags.selected {
        bg = colors.selection;
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
            selection: to_rgb(theme.selection),
            ansi: theme_ansi(&ThemeColors {
                dark: theme.is_dark(),
                background: to_rgb(theme.background),
                foreground: to_rgb(theme.foreground),
                muted: to_rgb(theme.muted),
                muted_foreground: to_rgb(theme.muted_foreground),
                red: (to_rgb(theme.red), to_rgb(theme.red_light)),
                green: (to_rgb(theme.green), to_rgb(theme.green_light)),
                yellow: (to_rgb(theme.yellow), to_rgb(theme.yellow_light)),
                blue: (to_rgb(theme.blue), to_rgb(theme.blue_light)),
                magenta: (to_rgb(theme.magenta), to_rgb(theme.magenta_light)),
                cyan: (to_rgb(theme.cyan), to_rgb(theme.cyan_light)),
            }),
        };
        let font = self.font.clone();
        let font_size = self.font_size;
        let focus = self.focus.clone();
        let prepaint_view = view.clone();
        let banner = self.clipboard_banner(cx);
        div()
            .id("terminal")
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(hsla(colors.background))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
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
                            let snapshot = view.emulator.snapshot();
                            view.display_offset = snapshot.display_offset;
                            (snapshot, view.marked.clone(), view.focus.is_focused(window))
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
            .children(banner)
    }
}

fn write_clipboard(write: ClipboardWrite, cx: &mut App) {
    let item = ClipboardItem::new_string(write.text);
    match write.target {
        ClipboardTarget::Clipboard => cx.write_to_clipboard(item),
        ClipboardTarget::Primary => cx.write_to_primary(item),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(openable_link("https://example.com/a?b=c"));
        assert!(openable_link("HTTP://example.com"));
        assert!(openable_link("mailto:me@example.com"));
        for uri in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://open",
            "ssh://host",
            "https://example.com/\nhttps://evil",
            "",
        ] {
            assert!(!openable_link(uri), "{uri}");
        }
    }

    #[test]
    fn previews_clipboard_text_on_one_line() {
        assert_eq!(clipboard_preview("a\nb\tc"), "a b c");
        let long = "한".repeat(CLIPBOARD_PREVIEW + 5);
        let preview = clipboard_preview(&long);
        assert_eq!(preview.chars().count(), CLIPBOARD_PREVIEW + 1);
        assert!(preview.ends_with('…'));
    }
}
