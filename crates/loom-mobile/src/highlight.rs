//! Syntax highlighting of files and diffs for the app.
//!
//! The grammars are bat's collection of TextMate grammars, run by syntect.
//! What a grammar matched is named by TextMate scopes such as
//! `keyword.control.rust`; those are mapped onto a few kinds of
//! [`Highlight`], so the app colors them from its own theme.

use std::ops::Range;
use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Parsing the grammars takes a while, so it happens once, when the first
/// file is highlighted.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// The most bytes of a text that are highlighted; the rest stays plain, so a
/// large file opens quickly.
const MAX_HIGHLIGHTED: usize = 1024 * 1024;
/// Lines longer than this, as in minified code, stay plain: the regular
/// expressions of a grammar can take long on them.
const MAX_LINE: usize = 4096;

/// What a part of a line is, for its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum Highlight {
    Comment,
    String,
    /// An escape in a string, such as `\n`.
    Escape,
    Number,
    /// A constant such as `true`, `None`, or a named one.
    Constant,
    Keyword,
    Operator,
    Type,
    Function,
    /// A markup tag, as in HTML.
    Tag,
    /// An attribute of a markup tag.
    Attribute,
    /// A field or a key, as in JSON or YAML.
    Property,
    /// A variable with a meaning of its own, such as `self`.
    Special,
    /// A heading, as in Markdown.
    Heading,
    Emphasis,
    Strong,
    Link,
    /// Inline code or a code block in Markdown.
    Code,
    Punctuation,
}

/// A highlighted part of a line, from `start` to `end` in UTF-16 code
/// units, as Kotlin strings count them.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Span {
    pub start: u32,
    pub end: u32,
    pub highlight: Highlight,
}

/// A line of code, without its line break, and its highlighted parts.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CodeLine {
    pub text: String,
    pub spans: Vec<Span>,
}

/// The grammar for a file: by its name, as for `Makefile`, its extension, or
/// its first line, as for a script with `#!/bin/sh`.
pub fn syntax_for(path: &str, text: &str) -> Option<&'static SyntaxReference> {
    let syntaxes = &*SYNTAXES;
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map(|(_, ext)| ext);
    let first_line = text.lines().next().unwrap_or_default();
    syntaxes
        .find_syntax_by_extension(name)
        .or_else(|| extension.and_then(|ext| syntaxes.find_syntax_by_extension(ext)))
        .or_else(|| syntaxes.find_syntax_by_first_line(first_line))
        .filter(|syntax| syntax.name != "Plain Text")
}

/// The lines of `text`, highlighted with `syntax` if there is one.
pub fn code_lines(text: &str, syntax: Option<&SyntaxReference>) -> Vec<CodeLine> {
    let lines: Vec<&str> = text.lines().collect();
    let spans = match syntax {
        Some(syntax) => highlight(syntax, &lines),
        None => vec![Vec::new(); lines.len()],
    };
    lines
        .into_iter()
        .zip(spans)
        .map(|(text, spans)| CodeLine {
            text: text.to_owned(),
            spans,
        })
        .collect()
}

/// The highlighted parts of each line, taking the lines as one text, so a
/// comment or string that spans lines is highlighted as one.
pub fn highlight(syntax: &SyntaxReference, lines: &[&str]) -> Vec<Vec<Span>> {
    let syntaxes = &*SYNTAXES;
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut budget = MAX_HIGHLIGHTED;
    let mut result = Vec::with_capacity(lines.len());
    let mut line_buf = String::new();
    for line in lines {
        if budget == 0 {
            result.push(Vec::new());
            continue;
        }
        budget = budget.saturating_sub(line.len() + 1);
        if line.len() > MAX_LINE {
            result.push(Vec::new());
            continue;
        }
        line_buf.clear();
        line_buf.push_str(line);
        line_buf.push('\n');
        let Ok(operations) = parser.parse_line(&line_buf, syntaxes) else {
            // The grammar failed on the line; the next one goes on from the
            // scopes it had.
            result.push(Vec::new());
            continue;
        };
        let mut ranges: Vec<(Range<usize>, Highlight)> = Vec::new();
        let mut cursor = 0;
        for (index, operation) in operations {
            push(&mut ranges, line, cursor..index.min(line.len()), &scopes);
            let _ = scopes.apply(&operation);
            cursor = index.min(line.len());
        }
        push(&mut ranges, line, cursor..line.len(), &scopes);
        result.push(utf16_spans(line, &ranges));
    }
    result
}

/// Adds the highlight of the text from `range` under `scopes`, joining it
/// to the one before when it is of the same kind.
fn push(
    ranges: &mut Vec<(Range<usize>, Highlight)>,
    line: &str,
    range: Range<usize>,
    scopes: &ScopeStack,
) {
    let Some(token) = line.get(range.clone()).filter(|t| !t.is_empty()) else {
        return;
    };
    // The innermost scope that is of a kind decides, so the quotes of a
    // string, which have none, are the string's.
    let found = scopes.as_slice().iter().rev().find_map(|s| kind(*s, token));
    let Some(highlight) = found else {
        return;
    };
    match ranges.last_mut() {
        Some((last, kind)) if last.end == range.start && *kind == highlight => {
            last.end = range.end;
        }
        _ => ranges.push((range, highlight)),
    }
}

/// Byte ranges of `line` as spans in UTF-16 code units.
fn utf16_spans(line: &str, ranges: &[(Range<usize>, Highlight)]) -> Vec<Span> {
    let mut spans = Vec::with_capacity(ranges.len());
    let mut units = 0u32;
    let mut chars = line.char_indices().peekable();
    let mut to_units = |target: usize| {
        while let Some(&(at, c)) = chars.peek() {
            if at >= target {
                break;
            }
            units += c.len_utf16() as u32;
            chars.next();
        }
        units
    };
    for (range, highlight) in ranges {
        let start = to_units(range.start);
        let end = to_units(range.end);
        if start < end {
            spans.push(Span {
                start,
                end,
                highlight: *highlight,
            });
        }
    }
    spans
}

/// The kind of a TextMate scope, or `None` when it has none of its own and
/// an enclosing scope decides. `token` is the text it covers, which tells a
/// declaration such as `fn` from a type such as `int`.
fn kind(scope: Scope, token: &str) -> Option<Highlight> {
    let name = scope.build_string();
    let is = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    // Containers and delimiters: the quotes of a string or the `//` of a
    // comment take the color of what they delimit.
    if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("meta")
        || is("source")
        || is("text")
        || is("invalid")
    {
        return None;
    }
    let kind = if is("comment") {
        Highlight::Comment
    } else if is("constant.character.escape") {
        Highlight::Escape
    } else if is("string") {
        Highlight::String
    } else if is("constant.numeric") {
        Highlight::Number
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        Highlight::Constant
    } else if is("keyword.operator") {
        Highlight::Operator
    } else if is("keyword") || is("storage") {
        // Grammars give `storage.type` to declarations, as `fn` or `class`,
        // and to types, as `int`.
        if is("storage.type") && !declares(token.trim()) {
            Highlight::Type
        } else {
            Highlight::Keyword
        }
    } else if is("entity.name.function")
        || is("support.function")
        || is("variable.function")
        || is("meta.function-call")
    {
        Highlight::Function
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.name.namespace")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        Highlight::Type
    } else if is("entity.name.tag") {
        Highlight::Tag
    } else if is("entity.other.attribute-name") {
        Highlight::Attribute
    } else if is("entity.name.section") || is("markup.heading") {
        Highlight::Heading
    } else if is("variable.language") {
        Highlight::Special
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
        || is("meta.mapping.key")
        || is("support.type.property-name")
    {
        Highlight::Property
    } else if is("markup.bold") {
        Highlight::Strong
    } else if is("markup.italic") {
        Highlight::Emphasis
    } else if is("markup.underline.link") || is("string.other.link") {
        Highlight::Link
    } else if is("markup.raw") {
        Highlight::Code
    } else if is("punctuation") {
        Highlight::Punctuation
    } else {
        return None;
    };
    Some(kind)
}

/// Whether a word is a keyword that declares something, as `fn` or `class`,
/// rather than the name of a type.
fn declares(word: &str) -> bool {
    matches!(
        word,
        "class"
            | "const"
            | "def"
            | "enum"
            | "fn"
            | "fun"
            | "func"
            | "function"
            | "impl"
            | "interface"
            | "let"
            | "mod"
            | "module"
            | "namespace"
            | "object"
            | "package"
            | "struct"
            | "trait"
            | "type"
            | "typealias"
            | "val"
            | "var"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &CodeLine) -> Vec<(String, Highlight)> {
        let units: Vec<u16> = line.text.encode_utf16().collect();
        line.spans
            .iter()
            .map(|s| {
                (
                    String::from_utf16(&units[s.start as usize..s.end as usize]).unwrap(),
                    s.highlight,
                )
            })
            .collect()
    }

    #[test]
    fn finds_grammars_by_name_extension_and_first_line() {
        assert_eq!(syntax_for("src/main.rs", "").unwrap().name, "Rust");
        assert_eq!(syntax_for("a/b/Makefile", "").unwrap().name, "Makefile");
        assert_eq!(syntax_for("app/Main.kt", "").unwrap().name, "Kotlin");
        assert_eq!(
            syntax_for("bin/run", "#!/bin/bash\necho hi").unwrap().name,
            "Bourne Again Shell (bash)"
        );
        assert!(syntax_for("notes.txt", "hello").is_none());
        assert!(syntax_for("LICENSE", "GNU GENERAL PUBLIC LICENSE").is_none());
    }

    #[test]
    fn highlights_keywords_strings_and_comments() {
        let text = "// says hi\nfn main() {\n    let s = \"héllo\\n\"; // 👋 x\n}\n";
        let lines = code_lines(text, syntax_for("main.rs", text));
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].text, "// says hi");
        assert_eq!(
            kinds(&lines[0]),
            [("// says hi".into(), Highlight::Comment)]
        );
        let second = kinds(&lines[1]);
        assert!(
            second.contains(&("fn".into(), Highlight::Keyword)),
            "{second:?}"
        );
        assert!(
            second.contains(&("main".into(), Highlight::Function)),
            "{second:?}"
        );
        let third = kinds(&lines[2]);
        assert!(
            third.contains(&("let".into(), Highlight::Keyword)),
            "{third:?}"
        );
        // Offsets count UTF-16 units: the string and the comment after the
        // emoji come out whole.
        assert!(
            third.contains(&("\"héllo".into(), Highlight::String)),
            "{third:?}"
        );
        assert!(
            third.contains(&("\\n".into(), Highlight::Escape)),
            "{third:?}"
        );
        assert!(
            third.contains(&("// 👋 x".into(), Highlight::Comment)),
            "{third:?}"
        );
    }

    #[test]
    fn a_comment_over_several_lines_stays_one() {
        let text = "/* one\ntwo */ int x;";
        let lines = code_lines(text, syntax_for("a.c", text));
        assert_eq!(kinds(&lines[0]), [("/* one".into(), Highlight::Comment)]);
        let second = kinds(&lines[1]);
        assert_eq!(second[0], ("two */".into(), Highlight::Comment));
        assert!(
            second.contains(&("int".into(), Highlight::Type)),
            "{second:?}"
        );
    }

    #[test]
    fn plain_text_and_long_lines_have_no_spans() {
        let lines = code_lines("just\ntext", None);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.spans.is_empty()));
        let long = format!("let x = \"{}\";\nlet y = 1;", "a".repeat(MAX_LINE));
        let lines = code_lines(&long, syntax_for("a.rs", &long));
        assert!(lines[0].spans.is_empty());
        assert!(!lines[1].spans.is_empty());
    }
}
