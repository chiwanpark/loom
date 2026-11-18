//! The colors that mark each host's sessions. A host's color is kept as a
//! `#rrggbb` text; the palette is what the clients offer to pick from.

/// The colors the clients offer, by name. They read well on dark and light
/// themes.
pub const PALETTE: [(&str, &str); 8] = [
    ("Red", "#ef4444"),
    ("Orange", "#f97316"),
    ("Yellow", "#eab308"),
    ("Green", "#22c55e"),
    ("Teal", "#14b8a6"),
    ("Blue", "#3b82f6"),
    ("Purple", "#a855f7"),
    ("Pink", "#ec4899"),
];

/// A `#rrggbb` color as `0xrrggbb`, or `None` when it is not one.
pub fn parse(color: &str) -> Option<u32> {
    let hex = color.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// The palette's name for `color`, if it is one of the palette's.
pub fn name(color: &str) -> Option<&'static str> {
    let rgb = parse(color)?;
    PALETTE
        .iter()
        .find(|(_, hex)| parse(hex) == Some(rgb))
        .map(|(name, _)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse("#3b82f6"), Some(0x3b82f6));
        assert_eq!(parse("#FFFFFF"), Some(0xffffff));
        for bad in ["3b82f6", "#3b82f", "#3b82f6a", "#gggggg", "", "#+12345"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_palette_is_valid_and_named() {
        for (label, hex) in PALETTE {
            assert!(parse(hex).is_some(), "{label}");
        }
        assert_eq!(name("#3B82F6"), Some("Blue"));
        assert_eq!(name("#123456"), None);
    }
}
