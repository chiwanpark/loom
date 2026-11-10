use loom_vt::input::{Key, Modifiers};

pub fn map_key(key: &str, mods: gpui::Modifiers) -> Option<(Key, Modifiers)> {
    let vt_mods = Modifiers {
        shift: mods.shift,
        alt: mods.alt,
        ctrl: mods.control,
        meta: false,
    };
    if mods.platform {
        return None;
    }
    let named = match key {
        "enter" => Some(Key::Enter),
        "tab" => Some(Key::Tab),
        "backspace" => Some(Key::Backspace),
        "escape" => Some(Key::Escape),
        "up" => Some(Key::Up),
        "down" => Some(Key::Down),
        "left" => Some(Key::Left),
        "right" => Some(Key::Right),
        "home" => Some(Key::Home),
        "end" => Some(Key::End),
        "pageup" => Some(Key::PageUp),
        "pagedown" => Some(Key::PageDown),
        "insert" => Some(Key::Insert),
        "delete" => Some(Key::Delete),
        _ => function_key(key),
    };
    if let Some(named) = named {
        return Some((named, vt_mods));
    }
    if !(mods.control || mods.alt) {
        return None;
    }
    let ch = match key {
        "space" => ' ',
        _ => {
            let mut chars = key.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            ch
        }
    };
    Some((Key::Char(ch), vt_mods))
}

fn function_key(key: &str) -> Option<Key> {
    let n: u8 = key.strip_prefix('f')?.parse().ok()?;
    (1..=24).contains(&n).then_some(Key::F(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(control: bool, alt: bool, shift: bool) -> gpui::Modifiers {
        gpui::Modifiers {
            control,
            alt,
            shift,
            ..Default::default()
        }
    }

    #[test]
    fn maps_named_and_control_keys() {
        assert_eq!(
            map_key("enter", mods(false, false, false)),
            Some((Key::Enter, Modifiers::default()))
        );
        assert_eq!(
            map_key("f5", mods(false, false, false)).unwrap().0,
            Key::F(5)
        );
        assert_eq!(map_key("fx", mods(false, false, false)), None);
        let (key, m) = map_key("c", mods(true, false, false)).unwrap();
        assert_eq!(key, Key::Char('c'));
        assert!(m.ctrl);
        assert_eq!(
            map_key("space", mods(true, false, false)).unwrap().0,
            Key::Char(' ')
        );
        assert!(map_key("tab", mods(false, false, true)).unwrap().1.shift);
    }

    #[test]
    fn leaves_text_to_the_input_handler() {
        assert_eq!(map_key("a", mods(false, false, false)), None);
        assert_eq!(map_key("a", mods(false, false, true)), None);
        assert_eq!(map_key("space", mods(false, false, false)), None);
        let platform = gpui::Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(map_key("c", platform), None);
    }
}
