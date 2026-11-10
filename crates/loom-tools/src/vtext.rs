use vte::{Params, Parser, Perform};

const TAB_WIDTH: usize = 8;

#[derive(Default)]
struct Line {
    cells: Vec<char>,
    col: usize,
    done: String,
}

impl Line {
    fn put(&mut self, c: char) {
        if self.col < self.cells.len() {
            self.cells[self.col] = c;
        } else {
            self.cells.resize(self.col, ' ');
            self.cells.push(c);
        }
        self.col += 1;
    }

    fn newline(&mut self) {
        let text: String = self.cells.iter().collect();
        self.done.push_str(text.trim_end());
        self.done.push('\n');
        self.cells.clear();
        self.col = 0;
    }
}

impl Perform for Line {
    fn print(&mut self, c: char) {
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => {
                let next = (self.col / TAB_WIDTH + 1) * TAB_WIDTH;
                while self.col < next {
                    self.put(' ');
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let first = params
            .iter()
            .next()
            .and_then(|p| p.first().copied())
            .unwrap_or(0) as usize;
        match action {
            'K' => match first {
                0 => self.cells.truncate(self.col),
                1 => {
                    for cell in self.cells.iter_mut().take(self.col + 1) {
                        *cell = ' ';
                    }
                }
                _ => self.cells.clear(),
            },
            'C' => self.col += first.max(1),
            'D' => self.col = self.col.saturating_sub(first.max(1)),
            'G' => self.col = first.max(1) - 1,
            'X' => {
                let end = (self.col + first.max(1)).min(self.cells.len());
                for cell in &mut self.cells[self.col.min(end)..end] {
                    *cell = ' ';
                }
            }
            _ => {}
        }
    }
}

pub struct PlainText {
    parser: Parser,
    line: Line,
}

impl Default for PlainText {
    fn default() -> Self {
        Self::new()
    }
}

impl PlainText {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
            line: Line::default(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.parser.advance(&mut self.line, bytes);
        std::mem::take(&mut self.line.done)
    }

    pub fn finish(&mut self) -> String {
        let mut out = std::mem::take(&mut self.line.done);
        if !self.line.cells.is_empty() {
            let text: String = self.line.cells.iter().collect();
            out.push_str(text.trim_end());
            self.line.cells.clear();
            self.line.col = 0;
        }
        out
    }

    pub fn render(bytes: &[u8]) -> String {
        let mut text = Self::new();
        let mut out = text.feed(bytes);
        out.push_str(&text.finish());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colors_and_resolves_overwrites() {
        assert_eq!(
            PlainText::render(b"\x1b[31mred\x1b[0m plain\r\n"),
            "red plain\n"
        );
        assert_eq!(
            PlainText::render(b"progress 10%\rprogress 50%\rprogress 100%\r\ndone"),
            "progress 100%\ndone"
        );
        assert_eq!(PlainText::render(b"abcdef\r\x1b[Kxy\n"), "xy\n");
        assert_eq!(PlainText::render(b"abc\x08\x08X\n"), "aXc\n");
        assert_eq!(PlainText::render(b"a\tb\n"), "a       b\n");
        assert_eq!(PlainText::render("한글 ✓\r\n".as_bytes()), "한글 ✓\n");
        assert_eq!(PlainText::render(b"\x1b]0;title\x07text\n"), "text\n");
    }

    #[test]
    fn streams_across_chunk_boundaries() {
        let mut text = PlainText::new();
        let mut out = text.feed(b"hel\x1b[3");
        out.push_str(&text.feed(b"2mlo\r"));
        out.push_str(&text.feed(b"\nworld"));
        out.push_str(&text.finish());
        assert_eq!(out, "hello\nworld");
    }
}
