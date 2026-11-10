use loom_vt::Color;

pub type Rgb = (u8, u8, u8);

const BASE: [Rgb; 16] = [
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

pub fn indexed(index: u8) -> Rgb {
    match index {
        0..=15 => BASE[index as usize],
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

pub fn resolve(color: Color, default: Rgb, bold: bool) -> Rgb {
    match color {
        Color::DefaultForeground | Color::DefaultBackground => default,
        Color::Indexed(i) if bold && i < 8 => indexed(i + 8),
        Color::Indexed(i) => indexed(i),
        Color::Rgb(r, g, b) => (r, g, b),
    }
}

pub fn dim(rgb: Rgb) -> Rgb {
    let f = |v: u8| ((v as u16 * 2) / 3) as u8;
    (f(rgb.0), f(rgb.1), f(rgb.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_the_xterm_cube_and_ramp() {
        assert_eq!(indexed(16), (0, 0, 0));
        assert_eq!(indexed(21), (0, 0, 255));
        assert_eq!(indexed(196), (255, 0, 0));
        assert_eq!(indexed(231), (255, 255, 255));
        assert_eq!(indexed(232), (8, 8, 8));
        assert_eq!(indexed(255), (238, 238, 238));
        assert_eq!(resolve(Color::Indexed(1), (0, 0, 0), true), BASE[9]);
        assert_eq!(
            resolve(Color::DefaultForeground, (1, 2, 3), false),
            (1, 2, 3)
        );
        assert_eq!(dim((90, 90, 90)), (60, 60, 60));
    }
}
