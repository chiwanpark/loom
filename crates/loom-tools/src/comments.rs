use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use similar::{DiffTag, TextDiff};

const DIFF_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REPORTED: usize = 5;
const MAX_TEXT: usize = 100;
const GUIDANCE: &str = "Do not add comments to code unless the user asks for them. \
You may change existing comments and add lines next to them, \
and you may add tool directives such as `eslint-disable`.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommentGuardMode {
    #[default]
    Off,
    Warn,
    Block,
}

#[derive(Debug, Clone)]
pub struct CommentGuard {
    pub mode: CommentGuardMode,
    pub allow_directives: bool,
    pub allow_patterns: Vec<Regex>,
    pub ignore_paths: Vec<String>,
}

impl Default for CommentGuard {
    fn default() -> Self {
        Self {
            mode: CommentGuardMode::Off,
            allow_directives: true,
            allow_patterns: Vec::new(),
            ignore_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileText<'a> {
    pub display: &'a str,
    pub old: &'a str,
    pub new: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewComment {
    pub path: String,
    pub line: usize,
    pub text: String,
}

impl CommentGuard {
    pub fn new(
        mode: CommentGuardMode,
        allow_directives: bool,
        allow_patterns: &[String],
        ignore_paths: Vec<String>,
    ) -> Result<Self, regex::Error> {
        Ok(Self {
            mode,
            allow_directives,
            allow_patterns: allow_patterns
                .iter()
                .map(|pattern| Regex::new(pattern))
                .collect::<Result<_, _>>()?,
            ignore_paths,
        })
    }

    pub fn mode(&self, session: Option<bool>) -> CommentGuardMode {
        match (session, self.mode) {
            (None, mode) => mode,
            (Some(false), _) => CommentGuardMode::Off,
            (Some(true), CommentGuardMode::Off) => CommentGuardMode::Block,
            (Some(true), mode) => mode,
        }
    }

    fn is_ignored(&self, display: &str) -> bool {
        self.ignore_paths.iter().any(|entry| {
            let entry = entry.trim_start_matches("./").trim_end_matches('/');
            !entry.is_empty()
                && (display == entry
                    || display
                        .strip_prefix(entry)
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    }

    fn is_allowed(&self, text: &str) -> bool {
        self.allow_patterns
            .iter()
            .any(|pattern| pattern.is_match(text))
            || (self.allow_directives && is_directive(text))
    }

    pub fn review(
        &self,
        session: Option<bool>,
        files: &[FileText<'_>],
    ) -> Result<Option<String>, String> {
        let mode = self.mode(session);
        if mode == CommentGuardMode::Off {
            return Ok(None);
        }
        let found = self.check(files);
        match (found.is_empty(), mode) {
            (true, _) => Ok(None),
            (false, CommentGuardMode::Block) => Err(rejection(&found)),
            (false, _) => Ok(Some(warning(&found))),
        }
    }

    pub fn check(&self, files: &[FileText<'_>]) -> Vec<NewComment> {
        let checked: Vec<(&FileText<'_>, &'static Syntax)> = files
            .iter()
            .filter(|file| !self.is_ignored(file.display))
            .filter_map(|file| syntax_of(Path::new(file.display)).map(|syntax| (file, syntax)))
            .collect();
        let mut known = Known::default();
        let scanned: Vec<_> = checked
            .iter()
            .map(|(file, syntax)| {
                let old = Scanned::new(syntax, file.old);
                known.add(&old);
                old
            })
            .collect();
        let mut found = Vec::new();
        for ((file, syntax), old) in checked.iter().zip(&scanned) {
            let new = Scanned::new(syntax, file.new);
            found.extend(
                self.new_comments(old, &new, &known)
                    .into_iter()
                    .map(|(line, text)| NewComment {
                        path: file.display.to_owned(),
                        line,
                        text,
                    }),
            );
        }
        found
    }

    fn new_comments(
        &self,
        old: &Scanned<'_>,
        new: &Scanned<'_>,
        known: &Known,
    ) -> Vec<(usize, String)> {
        let diff = TextDiff::configure()
            .timeout(DIFF_TIMEOUT)
            .diff_slices(&old.lines, &new.lines);
        let mut new_to_old = vec![None; new.lines.len()];
        let mut regions: Vec<Region> = Vec::new();
        let mut region_of = vec![None; new.lines.len()];
        let mut open = false;
        for op in diff.ops() {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                for (offset, line) in new_range.enumerate() {
                    new_to_old[line] = Some(old_range.start + offset);
                }
                open = false;
                continue;
            }
            if !open {
                regions.push(Region {
                    old: old_range.clone(),
                    new: new_range.clone(),
                    budget: 0,
                });
                open = true;
            }
            let region = regions.last_mut().expect("a region is open");
            region.old.end = old_range.end.max(region.old.end);
            region.new.end = new_range.end.max(region.new.end);
            for line in new_range {
                region_of[line] = Some(regions.len() - 1);
            }
        }
        let old_comment_lines = old.comment_lines();
        for region in &mut regions {
            region.budget = old
                .blocks()
                .into_iter()
                .filter(|block| overlaps(&old.span(block), &region.old))
                .count();
        }

        let mut found = Vec::new();
        for block in new.blocks() {
            let mut offending = Vec::new();
            for &index in &block {
                let unit = &new.units[index];
                let touched: Vec<usize> = (unit.start..=unit.end)
                    .filter(|&line| new_to_old[line].is_none())
                    .collect();
                let Some(&first) = touched.first() else {
                    continue;
                };
                if self.is_allowed(&unit.text) || known.has(unit, &touched, &new.lines) {
                    continue;
                }
                let text = if first == unit.start {
                    unit.text.clone()
                } else {
                    new.lines[first].trim().to_owned()
                };
                offending.push((first, text));
            }
            let Some(&(first, _)) = offending.first() else {
                continue;
            };
            let anchored = block.iter().any(|&index| {
                let unit = &new.units[index];
                (unit.start..=unit.end)
                    .filter_map(|line| new_to_old[line])
                    .any(|line| old_comment_lines[line])
            });
            if anchored {
                continue;
            }
            if let Some(region) = region_of[first].map(|index| &mut regions[index])
                && region.budget > 0
                && offending.iter().all(|(line, _)| region.new.contains(line))
            {
                region.budget -= 1;
                continue;
            }
            found.extend(offending.into_iter().map(|(line, text)| (line + 1, text)));
        }
        found
    }
}

struct Region {
    old: Range<usize>,
    new: Range<usize>,
    budget: usize,
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    if b.is_empty() {
        return false;
    }
    a.start < b.end && b.start < a.end
}

#[derive(Default)]
struct Known {
    lines: HashSet<String>,
    texts: HashSet<String>,
}

impl Known {
    fn add(&mut self, scanned: &Scanned<'_>) {
        for unit in &scanned.units {
            self.texts.insert(unit.text.clone());
            for line in &scanned.lines[unit.start..=unit.end] {
                self.lines.insert(line.trim().to_owned());
            }
        }
    }

    fn has(&self, unit: &Unit, touched: &[usize], lines: &[&str]) -> bool {
        let touched_lines = touched.iter().all(|&line| {
            let text = lines[line].trim();
            text.is_empty() || self.lines.contains(text)
        });
        touched_lines || (unit.start == unit.end && self.texts.contains(&unit.text))
    }
}

fn count(found: &[NewComment]) -> String {
    match found.len() {
        1 => "1 new comment".to_owned(),
        n => format!("{n} new comments"),
    }
}

fn rejection(found: &[NewComment]) -> String {
    format!(
        "The comment guard rejected the change: it adds {}.\n{}\n{GUIDANCE} \
         Remove the new comments and try again. \
         If the user asked for these comments, ask the user to turn off the comment guard for this session.",
        count(found),
        report(found)
    )
}

fn warning(found: &[NewComment]) -> String {
    format!(
        "\n\nThe comment guard found {}:\n{}\n{GUIDANCE} \
         Remove the new comments unless the user asked for them.",
        count(found),
        report(found)
    )
}

fn report(found: &[NewComment]) -> String {
    let mut lines: Vec<String> = found
        .iter()
        .take(MAX_REPORTED)
        .map(|comment| {
            let text = if comment.text.chars().count() > MAX_TEXT {
                let short: String = comment.text.chars().take(MAX_TEXT - 3).collect();
                format!("{short}...")
            } else {
                comment.text.clone()
            };
            format!("- {}:{}: {text}", comment.path, comment.line)
        })
        .collect();
    if found.len() > MAX_REPORTED {
        lines.push(format!("- ... and {} more", found.len() - MAX_REPORTED));
    }
    lines.join("\n")
}

struct Syntax {
    line_tokens: &'static [&'static str],
    block_tokens: &'static [(&'static str, &'static str)],
    quotes: &'static [char],
    multiline_quotes: &'static [char],
    docstrings: &'static [&'static str],
    char_literals: bool,
    boundary: bool,
}

const SCRIPT: Syntax = Syntax {
    line_tokens: &["//"],
    block_tokens: &[("/*", "*/")],
    quotes: &['"', '\''],
    multiline_quotes: &['`'],
    docstrings: &[],
    char_literals: false,
    boundary: false,
};

const C_LIKE: Syntax = Syntax {
    line_tokens: &["//"],
    block_tokens: &[("/*", "*/")],
    quotes: &['"'],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: true,
    boundary: false,
};

const RUST: Syntax = Syntax {
    line_tokens: &["//"],
    block_tokens: &[("/*", "*/")],
    quotes: &[],
    multiline_quotes: &['"'],
    docstrings: &[],
    char_literals: true,
    boundary: false,
};

const GO: Syntax = Syntax {
    line_tokens: &["//"],
    block_tokens: &[("/*", "*/")],
    quotes: &['"'],
    multiline_quotes: &['`'],
    docstrings: &[],
    char_literals: true,
    boundary: false,
};

const HASH: Syntax = Syntax {
    line_tokens: &["#"],
    block_tokens: &[],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: true,
};

const PYTHON: Syntax = Syntax {
    line_tokens: &["#"],
    block_tokens: &[],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &["\"\"\"", "'''"],
    char_literals: false,
    boundary: true,
};

const HASH_AND_C: Syntax = Syntax {
    line_tokens: &["#", "//"],
    block_tokens: &[("/*", "*/")],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: true,
};

const DASH: Syntax = Syntax {
    line_tokens: &["--"],
    block_tokens: &[],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: true,
};

const SQL: Syntax = Syntax {
    line_tokens: &["--"],
    block_tokens: &[("/*", "*/")],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: true,
};

const LUA: Syntax = Syntax {
    line_tokens: &["--"],
    block_tokens: &[("--[[", "]]")],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: true,
};

const MARKUP: Syntax = Syntax {
    line_tokens: &[],
    block_tokens: &[("<!--", "-->")],
    quotes: &[],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: false,
};

const CSS: Syntax = Syntax {
    line_tokens: &[],
    block_tokens: &[("/*", "*/")],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: false,
};

const SCSS: Syntax = Syntax {
    line_tokens: &["//"],
    block_tokens: &[("/*", "*/")],
    quotes: &['"', '\''],
    multiline_quotes: &[],
    docstrings: &[],
    char_literals: false,
    boundary: false,
};

fn syntax_of(path: &Path) -> Option<&'static Syntax> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let by_name = match name.as_str() {
        "dockerfile" | "makefile" | "gnumakefile" | "justfile" | "gemfile" | "rakefile"
        | "vagrantfile" | ".gitignore" | ".dockerignore" | ".gitattributes" | ".npmrc"
        | ".bashrc" | ".zshrc" | ".profile" => Some(&HASH),
        _ if name.starts_with("dockerfile") || name.starts_with("makefile") => Some(&HASH),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }
    let extension = name.rsplit_once('.')?.1;
    Some(match extension {
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "dart" | "groovy"
        | "gradle" | "jsonc" | "json5" => &SCRIPT,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "m" | "mm" | "java" | "kt"
        | "kts" | "scala" | "swift" | "cs" | "proto" | "zig" => &C_LIKE,
        "rs" => &RUST,
        "go" => &GO,
        "py" | "pyi" => &PYTHON,
        "sh" | "bash" | "zsh" | "fish" | "rb" | "pl" | "pm" | "r" | "yaml" | "yml" | "toml"
        | "ini" | "cfg" | "conf" | "nix" | "ps1" | "cmake" | "mk" => &HASH,
        "php" | "tf" | "tfvars" | "hcl" => &HASH_AND_C,
        "sql" => &SQL,
        "hs" | "elm" => &DASH,
        "lua" => &LUA,
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" | "md" | "mdx" => &MARKUP,
        "css" => &CSS,
        "scss" | "less" => &SCSS,
        _ => return None,
    })
}

static TEXT_DIRECTIVES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^#!",
        r"^/\*\s*globals?\s",
        r"^/\*\s*(?:eslint|jshint|jslint)\b",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid directive pattern"))
    .collect()
});

static DIRECTIVES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^-\*-",
        r"^(?:coding|encoding)[:=]",
        r"^eslint(?:-disable|-disable-line|-disable-next-line|-enable|-env)?\b",
        r"^(?:prettier-ignore|dprint-ignore|@format\b|@formatter:(?:off|on))",
        r"^(?:biome-ignore|oxlint-disable|deno-lint-ignore|deno-fmt-ignore|stylelint-(?:disable|enable))",
        r"^@ts-(?:ignore|expect-error|nocheck|check)\b",
        r"^@jsx(?:ImportSource)?\b",
        r"^(?:istanbul|c8|v8|node)\s+ignore\b",
        r"^SPDX-License-Identifier\b",
        r"^go:\w",
        r"^\+build\b",
        r"^nolint\b",
        r"^(?:noqa|nosec|NOSONAR)\b",
        r"^pragma\b",
        r"^type:\s*\S",
        r"(?i)^(?:mypy|pyright|pylint|ruff|flake8|isort|black|fmt|shellcheck|codeql|swiftlint|checkstyle|markdownlint|yamllint|hadolint|vale|editorconfig-checker):",
        r"^#?(?:region|endregion)\b",
        r"^<auto-generated",
        r"^@generated\b",
        r"^Code generated\b.*\bDO NOT EDIT",
        r"^frozen_string_literal:",
        r"^syntax=",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid directive pattern"))
    .collect()
});

static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:/{2,}!?|#+|-{2,}|/\*+!?|<!--|\*+)\s*").expect("valid marker pattern")
});

fn is_directive(text: &str) -> bool {
    if TEXT_DIRECTIVES.iter().any(|pattern| pattern.is_match(text)) {
        return true;
    }
    let body = MARKER.replace(text, "");
    DIRECTIVES.iter().any(|pattern| pattern.is_match(&body))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Unit {
    start: usize,
    end: usize,
    text: String,
    standalone: bool,
}

struct Scanned<'a> {
    lines: Vec<&'a str>,
    units: Vec<Unit>,
}

enum State {
    Code,
    Str(char),
    Block(&'static str, usize),
    Doc(&'static str, Option<usize>),
}

fn starts(chars: &[char], index: usize, token: &str) -> bool {
    token
        .chars()
        .enumerate()
        .all(|(offset, expected)| chars.get(index + offset) == Some(&expected))
}

fn find(chars: &[char], from: usize, token: &str) -> Option<usize> {
    (from..chars.len()).find(|&index| starts(chars, index, token))
}

fn close_quote(chars: &[char], from: usize, quote: char) -> Option<usize> {
    let mut index = from;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            c if c == quote => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

fn char_literal(chars: &[char], index: usize) -> Option<usize> {
    match chars.get(index + 1)? {
        '\\' => (index + 3..chars.len().min(index + 12))
            .find(|&end| chars[end] == '\'')
            .map(|end| end + 1 - index),
        _ => (chars.get(index + 2) == Some(&'\'')).then_some(3),
    }
}

fn text_of(chars: &[char], range: Range<usize>) -> String {
    chars[range].iter().collect::<String>().trim().to_owned()
}

impl<'a> Scanned<'a> {
    fn new(syntax: &Syntax, text: &'a str) -> Self {
        let lines: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            text.strip_suffix('\n')
                .unwrap_or(text)
                .split('\n')
                .collect()
        };
        let mut units: Vec<Unit> = Vec::new();
        let mut state = State::Code;
        let mut last_code: Option<&str> = None;

        for (number, line) in lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let mut index = 0;
            let mut code = false;
            match state {
                State::Code => {}
                State::Str(quote) => {
                    code = true;
                    match close_quote(&chars, 0, quote) {
                        Some(end) => {
                            index = end;
                            state = State::Code;
                        }
                        None => index = chars.len(),
                    }
                }
                State::Block(end, unit) => {
                    units[unit].end = number;
                    match find(&chars, 0, end) {
                        Some(position) => {
                            index = position + end.chars().count();
                            state = State::Code;
                        }
                        None => index = chars.len(),
                    }
                }
                State::Doc(quote, unit) => {
                    match unit {
                        Some(unit) => units[unit].end = number,
                        None => code = true,
                    }
                    match find(&chars, 0, quote) {
                        Some(position) => {
                            index = position + quote.chars().count();
                            state = State::Code;
                        }
                        None => index = chars.len(),
                    }
                }
            }

            while index < chars.len() {
                let c = chars[index];
                if c == '\\' {
                    code = true;
                    index += 2;
                    continue;
                }
                if let Some(&quote) = syntax
                    .docstrings
                    .iter()
                    .find(|quote| starts(&chars, index, quote))
                {
                    let prefix = text_of(&chars, 0..index);
                    let is_doc = prefix.len() <= 2
                        && prefix.chars().all(|c| "rbufRBUF".contains(c))
                        && last_code.is_none_or(|line| line.trim_end().ends_with(':'));
                    let unit = is_doc.then(|| {
                        units.push(Unit {
                            start: number,
                            end: number,
                            text: line.trim().to_owned(),
                            standalone: true,
                        });
                        units.len() - 1
                    });
                    code |= !is_doc;
                    let from = index + quote.chars().count();
                    match find(&chars, from, quote) {
                        Some(end) => index = end + quote.chars().count(),
                        None => {
                            state = State::Doc(quote, unit);
                            break;
                        }
                    }
                    continue;
                }
                if syntax.multiline_quotes.contains(&c) {
                    code = true;
                    match close_quote(&chars, index + 1, c) {
                        Some(end) => index = end,
                        None => {
                            state = State::Str(c);
                            break;
                        }
                    }
                    continue;
                }
                if syntax.quotes.contains(&c) {
                    code = true;
                    index = close_quote(&chars, index + 1, c).unwrap_or(chars.len());
                    continue;
                }
                if syntax.char_literals && c == '\'' {
                    code = true;
                    index += char_literal(&chars, index).unwrap_or(1);
                    continue;
                }
                if let Some(&(open, close)) = syntax
                    .block_tokens
                    .iter()
                    .find(|(open, _)| starts(&chars, index, open))
                {
                    let from = index + open.chars().count();
                    let end = find(&chars, from, close);
                    let text_end = end.map_or(chars.len(), |end| end + close.chars().count());
                    units.push(Unit {
                        start: number,
                        end: number,
                        text: text_of(&chars, index..text_end),
                        standalone: chars[..index].iter().all(|c| c.is_whitespace()),
                    });
                    match end {
                        Some(_) => index = text_end,
                        None => {
                            state = State::Block(close, units.len() - 1);
                            break;
                        }
                    }
                    continue;
                }
                let boundary = !syntax.boundary || index == 0 || chars[index - 1].is_whitespace();
                if boundary
                    && syntax
                        .line_tokens
                        .iter()
                        .any(|token| starts(&chars, index, token))
                {
                    units.push(Unit {
                        start: number,
                        end: number,
                        text: text_of(&chars, index..chars.len()),
                        standalone: chars[..index].iter().all(|c| c.is_whitespace()),
                    });
                    break;
                }
                if !c.is_whitespace() {
                    code = true;
                }
                index += 1;
            }
            if code {
                last_code = Some(line);
            }
        }
        Self { lines, units }
    }

    fn comment_lines(&self) -> Vec<bool> {
        let mut lines = vec![false; self.lines.len()];
        for unit in &self.units {
            lines[unit.start..=unit.end].fill(true);
        }
        lines
    }

    fn blocks(&self) -> Vec<Vec<usize>> {
        let mut blocks: Vec<Vec<usize>> = Vec::new();
        for (index, unit) in self.units.iter().enumerate() {
            if let Some(block) = blocks.last_mut()
                && let Some(&previous) = block.last()
                && unit.standalone
                && self.units[previous].standalone
                && unit.start <= self.units[previous].end + 1
            {
                block.push(index);
                continue;
            }
            blocks.push(vec![index]);
        }
        blocks
    }

    fn span(&self, block: &[usize]) -> Range<usize> {
        let first = &self.units[block[0]];
        let last = &self.units[block[block.len() - 1]];
        first.start..last.end + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> CommentGuard {
        CommentGuard {
            mode: CommentGuardMode::Block,
            ..CommentGuard::default()
        }
    }

    fn check(path: &str, old: &str, new: &str) -> Vec<(usize, String)> {
        guard()
            .check(&[FileText {
                display: path,
                old,
                new,
            }])
            .into_iter()
            .map(|comment| (comment.line, comment.text))
            .collect()
    }

    fn lines(path: &str, new: &str) -> Vec<usize> {
        check(path, "", new)
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }

    #[test]
    fn finds_line_and_block_comments() {
        assert_eq!(
            check(
                "a.ts",
                "",
                "const a = 1; // trailing\n// leading\nconst b = 2;\n"
            ),
            [(1, "// trailing".into()), (2, "// leading".into())]
        );
        assert_eq!(lines("a.ts", "/*\n * doc\n */\nexport const a = 1;\n"), [1]);
    }

    #[test]
    fn ignores_comment_markers_in_strings() {
        let text = "const url = \"https://example.com\";\nconst t = `a // b`;\nconst p = /https:\\/\\//;\nconst h = '#x';\n";
        assert!(lines("a.ts", text).is_empty());
        let rust =
            "fn f<'a>(x: &'a str) -> char {\n    let s = \"a\n// not a comment\";\n    '\"'\n}\n";
        assert!(lines("a.rs", rust).is_empty());
        assert_eq!(lines("a.rs", "fn f<'a>(x: &'a str) {} // note\n"), [1]);
    }

    #[test]
    fn allows_directives_and_patterns() {
        let text = "// eslint-disable-next-line no-console\n// @ts-expect-error legacy\n// compute the total\n";
        assert_eq!(lines("a.ts", text), [3]);
        assert!(lines("run.sh", "#!/usr/bin/env bash\necho \"${n#x}\" $#\n").is_empty());
        assert!(lines("a.py", "x = 1  # noqa: E501\n").is_empty());
        let custom = CommentGuard {
            allow_patterns: vec![Regex::new("TRACE:").unwrap()],
            ..guard()
        };
        let file = FileText {
            display: "a.ts",
            old: "",
            new: "// TRACE: keep\n",
        };
        assert!(custom.check(&[file]).is_empty());
        let strict = CommentGuard {
            allow_directives: false,
            ..guard()
        };
        let file = FileText {
            display: "a.ts",
            old: "",
            new: "// eslint-disable-next-line\n",
        };
        assert_eq!(strict.check(&[file]).len(), 1);
    }

    #[test]
    fn finds_comments_of_other_languages() {
        assert_eq!(lines("a.yaml", "key: value # note\n"), [1]);
        assert!(lines("a.yaml", "key: value\n").is_empty());
        assert_eq!(lines("a.py", "value = 1  # note\n"), [1]);
        assert!(lines("a.py", "value = \"# not\"\n").is_empty());
        assert_eq!(lines("q.sql", "select 1 -- note\n"), [1]);
        assert_eq!(lines("i.lua", "local a = 1 -- note\n"), [1]);
        assert_eq!(lines("p.html", "<div></div> <!-- note -->\n"), [1]);
        assert!(lines("README.md", "# Heading\n").is_empty());
        assert!(lines("data.json", "{\"a\": \"// x\"}\n").is_empty());
        assert!(lines("notes.txt", "// x\n").is_empty());
        assert_eq!(lines("Dockerfile", "# note\nFROM x\n"), [1]);
    }

    #[test]
    fn finds_python_docstrings() {
        assert_eq!(
            lines("a.py", "def run():\n    \"\"\"Run.\"\"\"\n    return 1\n"),
            [2]
        );
        assert_eq!(
            lines("a.py", "\"\"\"Module.\n\nMore.\n\"\"\"\nx = 1\n"),
            [1]
        );
        assert!(lines("a.py", "x = \"\"\"text\n# not a comment\n\"\"\"\n").is_empty());
    }

    #[test]
    fn allows_unchanged_and_moved_comments() {
        let old = "// keep\nfn a() {}\n\n/// Docs of b.\nfn b() {}\n";
        assert!(
            check(
                "a.rs",
                old,
                "// keep\nfn a() { x(); }\n\n/// Docs of b.\nfn b() {}\n"
            )
            .is_empty()
        );
        assert!(
            check(
                "a.rs",
                old,
                "/// Docs of b.\nfn b() {}\n\n// keep\nfn a() {}\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn allows_comments_that_extend_existing_ones() {
        let old = "/// Adds numbers.\nfn add() {}\n";
        let new = "/// Adds numbers.\n///\n/// Wraps on overflow.\nfn add() {}\n";
        assert!(check("a.rs", old, new).is_empty());
        let old = "/**\n * Adds numbers.\n */\nfn add() {}\n";
        let new = "/**\n * Adds numbers.\n * Wraps on overflow.\n */\nfn add() {}\n";
        assert!(check("a.rs", old, new).is_empty());
        let old = "def f():\n    \"\"\"Run.\n    \"\"\"\n";
        let new = "def f():\n    \"\"\"Run.\n\n    More text.\n    \"\"\"\n";
        assert!(check("a.py", old, new).is_empty());
    }

    #[test]
    fn allows_changes_to_existing_comments() {
        assert!(
            check(
                "a.rs",
                "// old words\nfn a() {}\n",
                "// new words\nfn a() {}\n"
            )
            .is_empty()
        );
        assert!(check("a.py", "x = 1  # old\n", "x = 2  # new\n").is_empty());
        assert!(check("a.py", "x = 1  # same\n", "x = 2  # same\n").is_empty());
    }

    #[test]
    fn rejects_new_comments_away_from_existing_ones() {
        let old = "// About a.\nfn a() {}\n\nfn b() {}\n";
        let new = "// About a.\nfn a() {}\n\n// About b.\nfn b() {}\n";
        assert_eq!(check("a.rs", old, new), [(4, "// About b.".into())]);
        assert_eq!(
            check("a.py", "x = 1\n", "x = 1  # new\n"),
            [(1, "# new".into())]
        );
        let old = "// One.\nfn a() {}\n";
        let new = "// Two.\nfn a() {}\n// Three.\nfn b() {}\n";
        assert_eq!(check("a.rs", old, new), [(3, "// Three.".into())]);
    }

    #[test]
    fn honors_ignored_paths_and_session_override() {
        let ignoring = CommentGuard {
            ignore_paths: vec!["vendor/".into()],
            ..guard()
        };
        let file = |display| FileText {
            display,
            old: "",
            new: "// x\n",
        };
        assert!(ignoring.check(&[file("vendor/a.rs")]).is_empty());
        assert_eq!(ignoring.check(&[file("vendorx/a.rs")]).len(), 1);

        let off = CommentGuard::default();
        assert_eq!(off.mode(None), CommentGuardMode::Off);
        assert_eq!(off.mode(Some(true)), CommentGuardMode::Block);
        assert_eq!(guard().mode(Some(false)), CommentGuardMode::Off);
        let warn = CommentGuard {
            mode: CommentGuardMode::Warn,
            ..guard()
        };
        assert_eq!(warn.mode(Some(true)), CommentGuardMode::Warn);
    }

    #[test]
    fn knows_comments_moved_between_files() {
        let files = [
            FileText {
                display: "a.rs",
                old: "/// Moved docs.\nfn m() {}\n",
                new: "",
            },
            FileText {
                display: "b.rs",
                old: "",
                new: "/// Moved docs.\nfn m() {}\n",
            },
        ];
        assert!(guard().check(&files).is_empty());
    }
}
