//! Syntax-highlighted code blocks for tool calls and results.

use std::ops::Range;

use gpui::{
    AnyElement, HighlightStyle, IntoElement, ParentElement, SharedString, Styled, StyledText, div,
    prelude::FluentBuilder, px,
};
use gpui_component::highlighter::{HighlightTheme, LanguageRegistry, SyntaxHighlighter};
use gpui_component::{Rope, Theme, h_flex, v_flex};

type Runs = Vec<(Range<usize>, HighlightStyle)>;

/// Highlighted text split into lines, each with highlight runs relative to the
/// line.
pub struct Highlighted {
    lines: Vec<(SharedString, Runs)>,
}

/// Highlights `text` as `language` (a name or alias such as `rs` or `bash`).
/// Languages without a grammar come back as plain lines.
pub fn highlight(text: &str, language: &str, theme: &HighlightTheme) -> Highlighted {
    let styles = LanguageRegistry::singleton()
        .language(language)
        .map(|config| {
            let mut highlighter = SyntaxHighlighter::new(&config.name);
            highlighter.update(None, &Rope::from_str(text), None);
            highlighter.styles(&(0..text.len()), theme)
        })
        .unwrap_or_default();
    split_lines(text, &styles)
}

/// Splits `text` into lines and cuts the runs (byte ranges into `text`) at
/// line boundaries, so each line keeps the runs that fall in it.
fn split_lines(text: &str, styles: &Runs) -> Highlighted {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut lines = Vec::new();
    let mut start = 0;
    let mut runs = styles.iter().peekable();
    for line in text.split('\n') {
        let end = start + line.len();
        let mut line_runs = Vec::new();
        while let Some((range, style)) = runs.peek() {
            if range.start >= end {
                break;
            }
            let (from, to) = (range.start.max(start), range.end.min(end));
            if from < to {
                line_runs.push((from - start..to - start, *style));
            }
            if range.end <= end {
                runs.next();
            } else {
                break;
            }
        }
        let shown = line.strip_suffix('\r').unwrap_or(line);
        line_runs.retain_mut(|(range, _)| {
            range.end = range.end.min(shown.len());
            range.start < range.end
        });
        lines.push((SharedString::from(shown.to_owned()), line_runs));
        start = end + 1;
    }
    Highlighted { lines }
}

/// Renders highlighted lines in a code block, with line numbers starting at
/// `first_line` when given.
pub fn code_view(code: &Highlighted, first_line: Option<u64>, theme: &Theme) -> AnyElement {
    let last = first_line.map(|first| first + code.lines.len().saturating_sub(1) as u64);
    let gutter = last.map(|last| last.to_string().len());
    v_flex()
        .w_full()
        .p_2()
        .rounded_sm()
        .bg(theme.muted)
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .children(code.lines.iter().enumerate().map(|(ix, (line, runs))| {
            // An empty text has no height; a space keeps the blank line.
            let text = if line.is_empty() {
                StyledText::new(" ")
            } else {
                StyledText::new(line.clone()).with_highlights(runs.iter().cloned())
            };
            h_flex()
                .items_start()
                .gap_3()
                .when_some(first_line.zip(gutter), |el, (first, width)| {
                    el.child(
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(format!("{:>width$}", first + ix as u64)),
                    )
                })
                .child(div().flex_1().min_w(px(0.)).child(text))
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style(n: u8) -> HighlightStyle {
        HighlightStyle {
            fade_out: Some(n as f32),
            ..Default::default()
        }
    }

    type Line<'a> = (&'a str, Vec<(Range<usize>, u8)>);

    fn lines(h: &Highlighted) -> Vec<Line<'_>> {
        h.lines
            .iter()
            .map(|(text, runs)| {
                let runs = runs
                    .iter()
                    .map(|(r, s)| (r.clone(), s.fade_out.unwrap() as u8))
                    .collect();
                (text.as_ref(), runs)
            })
            .collect()
    }

    #[test]
    fn runs_are_cut_at_line_boundaries() {
        // A run spanning the first two lines, and one on the "f" of the last.
        let text = "ab\ncd\n\nef\n";
        let styles = vec![(1..4, style(1)), (8..9, style(2))];
        let h = split_lines(text, &styles);
        assert_eq!(
            lines(&h),
            vec![
                ("ab", vec![(1..2, 1)]),
                ("cd", vec![(0..1, 1)]),
                ("", vec![]),
                ("ef", vec![(1..2, 2)]),
            ]
        );
    }

    #[test]
    fn highlights_languages_by_alias() {
        let theme = HighlightTheme::default_dark();
        for (code, language) in [("fn main() {}", "rs"), ("ls -la | head", "bash")] {
            let h = highlight(code, language, &theme);
            assert!(!h.lines[0].1.is_empty(), "{language}: no highlights");
        }
        // Unknown languages come back plain rather than failing.
        let h = highlight("a\nb", "no-such-language", &theme);
        assert_eq!(lines(&h), vec![("a", vec![]), ("b", vec![])]);
    }

    #[test]
    fn carriage_returns_are_dropped() {
        let h = split_lines("ab\r\ncd", &vec![(0..3, style(1))]);
        assert_eq!(lines(&h), vec![("ab", vec![(0..2, 1)]), ("cd", vec![])]);
    }
}
