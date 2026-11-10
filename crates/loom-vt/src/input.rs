use crate::Modes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
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
    F(u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub meta: bool,
}

impl Modifiers {
    fn param(self) -> u8 {
        1 + u8::from(self.shift)
            + 2 * u8::from(self.alt)
            + 4 * u8::from(self.ctrl)
            + 8 * u8::from(self.meta)
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl || self.meta
    }
}

fn with_alt(alt: bool, bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        let mut out = vec![0x1b];
        out.extend(bytes);
        out
    } else {
        bytes
    }
}

fn ctrl_char(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn cursor_key(final_byte: u8, mods: Modifiers, modes: Modes) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.param(), final_byte as char).into_bytes()
    } else if modes.app_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

fn tilde_key(code: u8, mods: Modifiers) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{code};{}~", mods.param()).into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

pub fn encode_key(key: Key, mods: Modifiers, modes: Modes) -> Option<Vec<u8>> {
    let bytes = match key {
        Key::Char(c) => {
            if mods.ctrl {
                let code = ctrl_char(c)?;
                with_alt(mods.alt, vec![code])
            } else {
                let mut buf = [0u8; 4];
                with_alt(mods.alt, c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        Key::Enter => with_alt(mods.alt, vec![b'\r']),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => with_alt(mods.alt, vec![b'\t']),
        Key::Backspace if mods.ctrl => with_alt(mods.alt, vec![0x08]),
        Key::Backspace => with_alt(mods.alt, vec![0x7f]),
        Key::Escape => with_alt(mods.alt, vec![0x1b]),
        Key::Up => cursor_key(b'A', mods, modes),
        Key::Down => cursor_key(b'B', mods, modes),
        Key::Right => cursor_key(b'C', mods, modes),
        Key::Left => cursor_key(b'D', mods, modes),
        Key::Home => cursor_key(b'H', mods, modes),
        Key::End => cursor_key(b'F', mods, modes),
        Key::Insert => tilde_key(2, mods),
        Key::Delete => tilde_key(3, mods),
        Key::PageUp => tilde_key(5, mods),
        Key::PageDown => tilde_key(6, mods),
        Key::F(n @ 1..=4) => {
            let final_byte = b'P' + (n - 1);
            if mods.any() {
                format!("\x1b[1;{}{}", mods.param(), final_byte as char).into_bytes()
            } else {
                vec![0x1b, b'O', final_byte]
            }
        }
        Key::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            tilde_key(code, mods)
        }
        Key::F(_) => return None,
    };
    Some(bytes)
}

pub fn encode_paste(text: &str, modes: Modes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if modes.bracketed_paste {
        let cleaned = normalized.replace("\x1b[201~", "");
        format!("\x1b[200~{cleaned}\x1b[201~").into_bytes()
    } else {
        normalized.into_bytes()
    }
}

pub fn encode_text(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key) -> Vec<u8> {
        encode_key(k, Modifiers::default(), Modes::default()).unwrap()
    }

    #[test]
    fn encodes_plain_and_control_keys() {
        assert_eq!(key(Key::Char('a')), b"a");
        assert_eq!(key(Key::Char('한')), "한".as_bytes());
        assert_eq!(key(Key::Enter), b"\r");
        assert_eq!(key(Key::Backspace), b"\x7f");
        assert_eq!(key(Key::Escape), b"\x1b");
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key(Key::Char('c'), ctrl, Modes::default()).unwrap(),
            b"\x03"
        );
        assert_eq!(
            encode_key(Key::Char('['), ctrl, Modes::default()).unwrap(),
            b"\x1b"
        );
        assert_eq!(
            encode_key(Key::Char(' '), ctrl, Modes::default()).unwrap(),
            b"\x00"
        );
        assert_eq!(encode_key(Key::Char('é'), ctrl, Modes::default()), None);
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key(Key::Char('b'), alt, Modes::default()).unwrap(),
            b"\x1bb"
        );
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_key(Key::Tab, shift, Modes::default()).unwrap(),
            b"\x1b[Z"
        );
    }

    #[test]
    fn encodes_cursor_and_function_keys() {
        assert_eq!(key(Key::Up), b"\x1b[A");
        let app = Modes {
            app_cursor: true,
            ..Modes::default()
        };
        assert_eq!(
            encode_key(Key::Up, Modifiers::default(), app).unwrap(),
            b"\x1bOA"
        );
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert_eq!(encode_key(Key::Right, ctrl, app).unwrap(), b"\x1b[1;5C");
        assert_eq!(key(Key::Delete), b"\x1b[3~");
        assert_eq!(
            encode_key(Key::PageUp, ctrl, Modes::default()).unwrap(),
            b"\x1b[5;5~"
        );
        assert_eq!(key(Key::F(1)), b"\x1bOP");
        assert_eq!(key(Key::F(5)), b"\x1b[15~");
        assert_eq!(key(Key::F(12)), b"\x1b[24~");
        assert_eq!(
            encode_key(Key::F(13), Modifiers::default(), Modes::default()),
            None
        );
    }

    #[test]
    fn brackets_pastes_when_requested() {
        assert_eq!(encode_paste("a\nb", Modes::default()), b"a\rb");
        let bracketed = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        assert_eq!(
            encode_paste("x\x1b[201~y", bracketed),
            b"\x1b[200~xy\x1b[201~"
        );
    }
}
