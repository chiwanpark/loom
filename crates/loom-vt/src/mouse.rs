use std::cmp;

use crate::input::Modifiers;
use crate::{GridPoint, Modes, Side};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub cell_width: f32,
    pub line_height: f32,
    pub cols: usize,
    pub rows: usize,
}

enum MouseFormat {
    Sgr,
    Normal(bool),
}

impl MouseFormat {
    fn from_modes(modes: Modes) -> Self {
        if modes.sgr_mouse {
            MouseFormat::Sgr
        } else {
            MouseFormat::Normal(modes.utf8_mouse)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ButtonCode {
    Left = 0,
    Middle = 1,
    Right = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
}

impl ButtonCode {
    fn press(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => ButtonCode::Left,
            MouseButton::Middle => ButtonCode::Middle,
            MouseButton::Right => ButtonCode::Right,
        }
    }

    fn motion(button: Option<MouseButton>) -> Self {
        match button {
            Some(MouseButton::Left) => ButtonCode::LeftMove,
            Some(MouseButton::Middle) => ButtonCode::MiddleMove,
            Some(MouseButton::Right) => ButtonCode::RightMove,
            None => ButtonCode::NoneMove,
        }
    }
}

pub fn point_and_side(
    x: f32,
    y: f32,
    metrics: CellMetrics,
    display_offset: usize,
) -> (GridPoint, Side) {
    let cell_width = metrics.cell_width.max(1.0);
    let line_height = metrics.line_height.max(1.0);
    let mut column = (x / cell_width).max(0.0) as usize;
    let cell_x = x.max(0.0) % cell_width;
    let mut side = if cell_x > cell_width / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    let last_column = metrics.cols.saturating_sub(1);
    if column > last_column {
        column = last_column;
        side = Side::Right;
    }
    let column = cmp::min(column, last_column);
    let mut line = (y / line_height).floor() as i32;
    let bottom = i32::try_from(metrics.rows.saturating_sub(1)).unwrap_or(i32::MAX);
    if line > bottom {
        line = bottom;
        side = Side::Right;
    } else if line < 0 {
        side = Side::Left;
    }
    let offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    (
        GridPoint {
            line: line.saturating_sub(offset),
            column,
        },
        side,
    )
}

pub fn wants_mouse(modes: Modes, shift: bool) -> bool {
    modes.mouse_reporting && !shift
}

pub fn button_report(
    point: GridPoint,
    button: MouseButton,
    mods: Modifiers,
    pressed: bool,
    modes: Modes,
) -> Option<Vec<u8>> {
    if !modes.mouse_reporting {
        return None;
    }
    report(
        point,
        ButtonCode::press(button),
        pressed,
        mods,
        MouseFormat::from_modes(modes),
    )
}

pub fn motion_report(
    point: GridPoint,
    button: Option<MouseButton>,
    mods: Modifiers,
    modes: Modes,
) -> Option<Vec<u8>> {
    if !(modes.mouse_motion || modes.mouse_drag) {
        return None;
    }
    let code = ButtonCode::motion(button);
    if !modes.mouse_motion && matches!(code, ButtonCode::NoneMove) {
        return None;
    }
    report(point, code, true, mods, MouseFormat::from_modes(modes))
}

pub fn scroll_report(
    point: GridPoint,
    lines: i32,
    mods: Modifiers,
    modes: Modes,
) -> Option<Vec<Vec<u8>>> {
    if !modes.mouse_reporting || lines == 0 {
        return None;
    }
    let code = if lines > 0 {
        ButtonCode::ScrollUp
    } else {
        ButtonCode::ScrollDown
    };
    let one = report(point, code, true, mods, MouseFormat::from_modes(modes))?;
    Some(vec![one; lines.unsigned_abs() as usize])
}

pub fn alt_scroll(lines: i32) -> Vec<u8> {
    let cmd = if lines > 0 { b'A' } else { b'B' };
    let mut content = Vec::with_capacity(lines.unsigned_abs() as usize * 3);
    for _ in 0..lines.unsigned_abs() {
        content.extend_from_slice(&[0x1b, b'O', cmd]);
    }
    content
}

fn report(
    point: GridPoint,
    code: ButtonCode,
    pressed: bool,
    mods: Modifiers,
    format: MouseFormat,
) -> Option<Vec<u8>> {
    if point.line < 0 {
        return None;
    }
    let mut modifier = 0;
    if mods.shift {
        modifier += 4;
    }
    if mods.alt {
        modifier += 8;
    }
    if mods.ctrl {
        modifier += 16;
    }
    match format {
        MouseFormat::Sgr => Some(sgr_report(point, code as u8 + modifier, pressed).into_bytes()),
        MouseFormat::Normal(utf8) => {
            if pressed {
                normal_report(point, code as u8 + modifier, utf8)
            } else {
                normal_report(point, 3 + modifier, utf8)
            }
        }
    }
}

fn normal_report(point: GridPoint, button: u8, utf8: bool) -> Option<Vec<u8>> {
    let max_point = if utf8 { 2015 } else { 223 };
    if point.line >= max_point || point.column >= max_point as usize {
        return None;
    }
    let mut message = vec![0x1b, b'[', b'M', 32 + button];
    let encode = |pos: usize| -> [u8; 2] {
        let pos = 32 + 1 + pos;
        [(0xC0 + pos / 64) as u8, (0x80 + (pos & 63)) as u8]
    };
    if utf8 && point.column >= 95 {
        message.extend_from_slice(&encode(point.column));
    } else {
        message.push(32 + 1 + point.column as u8);
    }
    if utf8 && point.line >= 95 {
        message.extend_from_slice(&encode(point.line as usize));
    } else {
        message.push(32 + 1 + point.line as u8);
    }
    Some(message)
}

fn sgr_report(point: GridPoint, button: u8, pressed: bool) -> String {
    let kind = if pressed { 'M' } else { 'm' };
    format!(
        "\x1b[<{};{};{}{}",
        button,
        point.column + 1,
        point.line + 1,
        kind
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(sgr: bool) -> Modes {
        Modes {
            mouse_reporting: true,
            sgr_mouse: sgr,
            ..Modes::default()
        }
    }

    fn at(line: i32, column: usize) -> GridPoint {
        GridPoint { line, column }
    }

    #[test]
    fn encodes_sgr_press_and_release() {
        let mods = Modifiers::default();
        let press = button_report(at(2, 4), MouseButton::Left, mods, true, modes(true)).unwrap();
        assert_eq!(press, b"\x1b[<0;5;3M");
        let release = button_report(at(2, 4), MouseButton::Left, mods, false, modes(true)).unwrap();
        assert_eq!(release, b"\x1b[<0;5;3m");
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let right = button_report(at(0, 0), MouseButton::Right, ctrl, true, modes(true)).unwrap();
        assert_eq!(right, b"\x1b[<18;1;1M");
    }

    #[test]
    fn encodes_normal_reports() {
        let mods = Modifiers::default();
        let press = button_report(at(0, 0), MouseButton::Left, mods, true, modes(false)).unwrap();
        assert_eq!(press, vec![0x1b, b'[', b'M', 32, 33, 33]);
        let release =
            button_report(at(0, 0), MouseButton::Left, mods, false, modes(false)).unwrap();
        assert_eq!(release, vec![0x1b, b'[', b'M', 35, 33, 33]);
        assert!(button_report(at(300, 0), MouseButton::Left, mods, true, modes(false)).is_none());
    }

    #[test]
    fn reports_nothing_without_mouse_mode() {
        let mods = Modifiers::default();
        let off = Modes::default();
        assert!(button_report(at(0, 0), MouseButton::Left, mods, true, off).is_none());
        assert!(scroll_report(at(0, 0), 3, mods, off).is_none());
        assert!(motion_report(at(0, 0), None, mods, modes(true)).is_none());
    }

    #[test]
    fn repeats_scroll_reports_and_alt_scroll() {
        let mods = Modifiers::default();
        let up = scroll_report(at(1, 1), 3, mods, modes(true)).unwrap();
        assert_eq!(up.len(), 3);
        assert_eq!(up[0], b"\x1b[<64;2;2M");
        let down = scroll_report(at(1, 1), -2, mods, modes(true)).unwrap();
        assert_eq!(down[1], b"\x1b[<65;2;2M");
        assert_eq!(alt_scroll(-2), b"\x1bOB\x1bOB");
    }

    #[test]
    fn drag_reports_only_with_a_button_held() {
        let mods = Modifiers::default();
        let drag = Modes {
            mouse_drag: true,
            ..modes(true)
        };
        assert!(motion_report(at(0, 0), None, mods, drag).is_none());
        assert_eq!(
            motion_report(at(0, 0), Some(MouseButton::Left), mods, drag).unwrap(),
            b"\x1b[<32;1;1M"
        );
    }

    #[test]
    fn maps_pixels_to_cells_and_sides() {
        let metrics = CellMetrics {
            cell_width: 10.0,
            line_height: 20.0,
            cols: 80,
            rows: 24,
        };
        assert_eq!(
            point_and_side(26.0, 45.0, metrics, 0),
            (at(2, 2), Side::Right)
        );
        assert_eq!(
            point_and_side(21.0, 5.0, metrics, 3),
            (at(-3, 2), Side::Left)
        );
        assert_eq!(
            point_and_side(5000.0, 5000.0, metrics, 0),
            (at(23, 79), Side::Right)
        );
    }
}
