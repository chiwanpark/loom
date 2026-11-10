use loom_vt::Color;

pub type Rgb = (u8, u8, u8);

pub type Ansi = [Rgb; 16];

pub const DEFAULT_ANSI: Ansi = [
    (0x1d, 0x1f, 0x21),
    (0xcc, 0x66, 0x66),
    (0xb5, 0xbd, 0x68),
    (0xf0, 0xc6, 0x74),
    (0x81, 0xa2, 0xbe),
    (0xb2, 0x94, 0xbb),
    (0x8a, 0xbe, 0xb7),
    (0xc5, 0xc8, 0xc6),
    (0x66, 0x66, 0x66),
    (0xd5, 0x4e, 0x53),
    (0xb9, 0xca, 0x4a),
    (0xe7, 0xc5, 0x47),
    (0x7a, 0xa6, 0xda),
    (0xc3, 0x97, 0xd8),
    (0x70, 0xc0, 0xb1),
    (0xea, 0xea, 0xea),
];

pub fn indexed(index: u8, ansi: &Ansi) -> Rgb {
    match index {
        0..=15 => ansi[index as usize],
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => {
            let v = 8 + (index - 232) * 10;
            (v, v, v)
        }
    }
}

pub fn resolve(color: Color, default: Rgb, bold: bool, ansi: &Ansi) -> Rgb {
    match color {
        Color::DefaultForeground | Color::DefaultBackground => default,
        Color::Indexed(i) if bold && i < 8 => indexed(i + 8, ansi),
        Color::Indexed(i) => indexed(i, ansi),
        Color::Rgb(r, g, b) => (r, g, b),
    }
}

pub struct ThemeColors {
    pub dark: bool,
    pub background: Rgb,
    pub foreground: Rgb,
    pub muted: Rgb,
    pub muted_foreground: Rgb,
    pub red: (Rgb, Rgb),
    pub green: (Rgb, Rgb),
    pub yellow: (Rgb, Rgb),
    pub blue: (Rgb, Rgb),
    pub magenta: (Rgb, Rgb),
    pub cyan: (Rgb, Rgb),
}

pub fn theme_ansi(theme: &ThemeColors) -> Ansi {
    let (black, white, bright_white) = if theme.dark {
        (
            theme.muted,
            dim_by(theme.foreground, 5, 6),
            theme.foreground,
        )
    } else {
        (theme.foreground, theme.muted, theme.background)
    };
    [
        black,
        theme.red.0,
        theme.green.0,
        theme.yellow.0,
        theme.blue.0,
        theme.magenta.0,
        theme.cyan.0,
        white,
        theme.muted_foreground,
        theme.red.1,
        theme.green.1,
        theme.yellow.1,
        theme.blue.1,
        theme.magenta.1,
        theme.cyan.1,
        bright_white,
    ]
}

fn dim_by(rgb: Rgb, num: u16, den: u16) -> Rgb {
    let f = |v: u8| ((v as u16 * num) / den) as u8;
    (f(rgb.0), f(rgb.1), f(rgb.2))
}

pub fn dim(rgb: Rgb) -> Rgb {
    dim_by(rgb, 2, 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_the_xterm_cube_and_ramp() {
        assert_eq!(indexed(16, &DEFAULT_ANSI), (0, 0, 0));
        assert_eq!(indexed(21, &DEFAULT_ANSI), (0, 0, 255));
        assert_eq!(indexed(196, &DEFAULT_ANSI), (255, 0, 0));
        assert_eq!(indexed(231, &DEFAULT_ANSI), (255, 255, 255));
        assert_eq!(indexed(232, &DEFAULT_ANSI), (8, 8, 8));
        assert_eq!(indexed(255, &DEFAULT_ANSI), (238, 238, 238));
        assert_eq!(
            resolve(Color::Indexed(1), (0, 0, 0), true, &DEFAULT_ANSI),
            DEFAULT_ANSI[9]
        );
        assert_eq!(
            resolve(Color::DefaultForeground, (1, 2, 3), false, &DEFAULT_ANSI),
            (1, 2, 3)
        );
        assert_eq!(dim((90, 90, 90)), (60, 60, 60));
    }

    fn colors(dark: bool) -> ThemeColors {
        ThemeColors {
            dark,
            background: (10, 10, 10),
            foreground: (240, 240, 240),
            muted: (40, 40, 40),
            muted_foreground: (128, 128, 128),
            red: ((200, 0, 0), (255, 80, 80)),
            green: ((0, 200, 0), (80, 255, 80)),
            yellow: ((200, 200, 0), (255, 255, 80)),
            blue: ((0, 0, 200), (80, 80, 255)),
            magenta: ((200, 0, 200), (255, 80, 255)),
            cyan: ((0, 200, 200), (80, 255, 255)),
        }
    }

    #[test]
    fn maps_theme_colors_to_ansi_slots() {
        let dark = theme_ansi(&colors(true));
        assert_eq!(dark[0], (40, 40, 40));
        assert_eq!(dark[1], (200, 0, 0));
        assert_eq!(dark[9], (255, 80, 80));
        assert_eq!(dark[8], (128, 128, 128));
        assert_eq!(dark[7], (200, 200, 200));
        assert_eq!(dark[15], (240, 240, 240));
        let light = theme_ansi(&colors(false));
        assert_eq!(light[0], (240, 240, 240));
        assert_eq!(light[7], (40, 40, 40));
        assert_eq!(light[15], (10, 10, 10));
        assert_eq!(light[14], (80, 255, 255));
    }
}
