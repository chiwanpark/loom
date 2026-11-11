//! Composition of decomposed (NFD) Hangul for display.
//!
//! File names from macOS store Hangul as conjoining jamo: a leading
//! consonant, a vowel, and an optional trailing consonant. The terminal model
//! keeps such a syllable in one wide cell as the leading jamo plus zero-width
//! characters, and fonts rarely join the jamo into one glyph. Composing them
//! into the precomposed syllable (as Unicode canonical composition does for
//! Hangul) makes the cell show what an IME would have typed.
//!
//! Only Hangul is composed. Full NFC would also replace other characters,
//! such as CJK compatibility ideographs, with different glyphs.

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

fn is_jamo(c: char) -> bool {
    matches!(c as u32, 0x1100..=0x11FF)
}

/// Joins a leading jamo and a vowel, or an LV syllable and a trailing jamo.
fn join(first: char, second: char) -> Option<char> {
    let (a, b) = (first as u32, second as u32);
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return char::from_u32(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    let s = a.wrapping_sub(S_BASE);
    if s < S_COUNT && s % T_COUNT == 0 && (T_BASE + 1..T_BASE + T_COUNT).contains(&b) {
        return char::from_u32(a + (b - T_BASE));
    }
    None
}

/// The text with conjoining Hangul jamo composed into syllables, or `None`
/// when nothing changes.
pub fn compose(text: &str) -> Option<String> {
    if !text.chars().any(is_jamo) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut last: Option<char> = None;
    let mut changed = false;
    for c in text.chars() {
        if let Some(joined) = last.and_then(|prev| join(prev, c)) {
            last = Some(joined);
            changed = true;
            continue;
        }
        out.extend(last.replace(c));
    }
    out.extend(last);
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_decomposed_syllables() {
        // "한글" as macOS stores it: ᄒ ᅡ ᆫ ᄀ ᅳ ᆯ.
        assert_eq!(
            compose("\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}").as_deref(),
            Some("한글")
        );
        assert_eq!(compose("\u{1111}\u{1161}").as_deref(), Some("파"));
        assert_eq!(compose("\u{ac00}\u{11a8}").as_deref(), Some("각"));
        // The first and last syllables of the block.
        assert_eq!(compose("\u{1100}\u{1161}").as_deref(), Some("\u{ac00}"));
        assert_eq!(
            compose("\u{1112}\u{1175}\u{11c2}").as_deref(),
            Some("\u{d7a3}")
        );
    }

    #[test]
    fn leaves_other_text_alone() {
        assert_eq!(compose("plain ascii"), None);
        assert_eq!(compose("한글"), None);
        // Full NFC would turn this compatibility ideograph into U+8C48.
        assert_eq!(compose("\u{f900}"), None);
        assert_eq!(compose("e\u{301}"), None);
        // Old jamo outside the modern syllable block have no precomposed form.
        assert_eq!(compose("\u{1113}\u{1161}"), None);
        // A syllable that already has a trailing consonant takes no other.
        assert_eq!(compose("\u{ac01}\u{11a8}"), None);
        // A lone vowel or trailing jamo stays as it is.
        assert_eq!(compose("\u{1161}"), None);
        assert_eq!(compose("a\u{11a8}"), None);
    }

    #[test]
    fn keeps_uncomposable_jamo_next_to_composed_ones() {
        assert_eq!(
            compose("\u{1100}\u{1161}\u{1161}x").as_deref(),
            Some("\u{ac00}\u{1161}x")
        );
    }
}
