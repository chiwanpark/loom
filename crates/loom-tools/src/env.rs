//! The environment of the commands that the agent runs, and the redaction of
//! secret values in what the model sees.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aho_corasick::{AhoCorasick, MatchKind};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Values shorter than this are not redacted: they would match too much of
/// the output.
pub const MIN_SECRET_LEN: usize = 8;

/// What URL encoding escapes: all but the unreserved characters.
const URL_ESCAPED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The environment of a command, built for each tool call.
#[derive(Debug, Default)]
pub struct CommandEnv {
    /// Every variable the command gets, in place of the agent's own.
    pub vars: Vec<(OsString, OsString)>,
    /// Hides the secret values in the command's output.
    pub redactor: Redactor,
    /// Files with secret values, such as the env file: the read tool hides
    /// their values, the write and edit tools refuse them, and the bubblewrap
    /// sandbox hides them from commands.
    pub protected: Vec<PathBuf>,
    /// Names of variables whose values the read tool shows in protected files.
    pub shown: HashSet<String>,
}

impl CommandEnv {
    /// Whether `path`, an absolute path, is one of the protected files.
    pub fn is_protected(&self, path: &Path) -> bool {
        let canonical = std::fs::canonicalize(path).ok();
        self.protected
            .iter()
            .any(|protected| protected == path || Some(protected) == canonical.as_ref())
    }
}

/// Replaces known secret values, and their base64 and URL-encoded forms,
/// with `[REDACTED:<NAME>]`.
#[derive(Clone, Default)]
pub struct Redactor {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    matcher: AhoCorasick,
    replacements: Vec<Vec<u8>>,
    longest: usize,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let patterns = self.inner.as_ref().map_or(0, |i| i.replacements.len());
        f.debug_struct("Redactor")
            .field("patterns", &patterns)
            .finish()
    }
}

impl Redactor {
    /// A redactor for the values of `secrets`, pairs of a name and a value.
    /// Values shorter than [`MIN_SECRET_LEN`] are left out.
    pub fn new<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut seen = HashSet::new();
        let mut patterns: Vec<Vec<u8>> = Vec::new();
        let mut replacements = Vec::new();
        for (name, value) in secrets {
            if value.len() < MIN_SECRET_LEN {
                continue;
            }
            let replacement = format!("[REDACTED:{name}]").into_bytes();
            for form in forms(value) {
                if form.len() >= MIN_SECRET_LEN && seen.insert(form.clone()) {
                    patterns.push(form);
                    replacements.push(replacement.clone());
                }
            }
        }
        if patterns.is_empty() {
            return Self::default();
        }
        let longest = patterns.iter().map(Vec::len).max().unwrap_or(0);
        let matcher = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .expect("the patterns are few and short enough to build");
        Self {
            inner: Some(Arc::new(Inner {
                matcher,
                replacements,
                longest,
            })),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_none()
    }

    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.redact_bytes(text.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(text),
            Cow::Owned(bytes) => Cow::Owned(String::from_utf8_lossy(&bytes).into_owned()),
        }
    }

    pub fn redact_bytes<'a>(&self, data: &'a [u8]) -> Cow<'a, [u8]> {
        let Some(inner) = &self.inner else {
            return Cow::Borrowed(data);
        };
        let mut matches = inner.matcher.find_iter(data).peekable();
        if matches.peek().is_none() {
            return Cow::Borrowed(data);
        }
        let mut out = Vec::with_capacity(data.len());
        let mut pos = 0;
        for found in matches {
            out.extend_from_slice(&data[pos..found.start()]);
            out.extend_from_slice(&inner.replacements[found.pattern().as_usize()]);
            pos = found.end();
        }
        out.extend_from_slice(&data[pos..]);
        Cow::Owned(out)
    }

    /// A redactor for output that comes in chunks, which finds a value that
    /// is split over two chunks too.
    pub fn stream(&self) -> StreamRedactor {
        StreamRedactor {
            redactor: self.clone(),
            carry: Vec::new(),
        }
    }
}

/// Redacts a stream of chunks. It holds back the end of each chunk that could
/// be the start of a value until the next chunk or [`StreamRedactor::finish`].
#[derive(Debug)]
pub struct StreamRedactor {
    redactor: Redactor,
    carry: Vec<u8>,
}

impl StreamRedactor {
    /// The redacted output that is sure so far.
    pub fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let Some(inner) = self.redactor.inner.clone() else {
            return data.to_vec();
        };
        let mut buffer = std::mem::take(&mut self.carry);
        buffer.extend_from_slice(data);
        // A value that starts before `safe` ends in the buffer.
        let safe = buffer.len().saturating_sub(inner.longest - 1);
        let mut out = Vec::with_capacity(buffer.len());
        let mut pos = 0;
        for found in inner.matcher.find_iter(&buffer) {
            if found.start() >= safe {
                break;
            }
            out.extend_from_slice(&buffer[pos..found.start()]);
            out.extend_from_slice(&inner.replacements[found.pattern().as_usize()]);
            pos = found.end();
        }
        let keep = pos.max(safe);
        out.extend_from_slice(&buffer[pos..keep]);
        self.carry = buffer.split_off(keep);
        out
    }

    /// The rest of the output, at its end.
    pub fn finish(&mut self) -> Vec<u8> {
        let carry = std::mem::take(&mut self.carry);
        self.redactor.redact_bytes(&carry).into_owned()
    }
}

/// The forms of `value` that output may show: the value, its lines, its
/// base64 encodings at each alignment in a longer text, and its URL encoding.
fn forms(value: &str) -> Vec<Vec<u8>> {
    let mut forms = vec![value.as_bytes().to_vec()];
    if value.contains('\n') {
        forms.extend(
            value
                .lines()
                .map(|line| line.trim_end_matches('\r').as_bytes().to_vec()),
        );
    }
    for engine in [&STANDARD, &URL_SAFE] {
        forms.push(engine.encode(value).into_bytes());
        for offset in 0..3 {
            let mut padded = vec![0u8; offset];
            padded.extend_from_slice(value.as_bytes());
            let encoded = engine.encode(&padded);
            // The characters that only the value's bytes make.
            let start = if offset == 0 { 0 } else { 4 };
            let end = padded.len() / 3 * 4;
            if end > start {
                forms.push(encoded.as_bytes()[start..end].to_vec());
            }
        }
    }
    let url = utf8_percent_encode(value, URL_ESCAPED).to_string();
    if url != value {
        forms.push(url.into_bytes());
    }
    forms
}

/// Parses a dotenv file. On an error, tells which line it is at, without the
/// line's text, which may hold a secret.
pub fn parse_dotenv(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut vars = Vec::new();
    for item in dotenvy::from_read_iter(text.as_bytes()) {
        match item {
            Ok(pair) => vars.push(pair),
            Err(dotenvy::Error::LineParse(line, _)) => {
                let first = line.lines().next().unwrap_or_default();
                return Err(
                    match text.lines().position(|l| l.trim_end() == first.trim_end()) {
                        Some(index) => format!("line {} cannot be parsed", index + 1),
                        None => "a line cannot be parsed".into(),
                    },
                );
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(vars)
}

/// The text of a dotenv file with the values hidden, except those of the
/// variables in `shown`. Comments and blank lines stay, and every line keeps
/// its place.
pub fn mask_dotenv(text: &str, shown: &HashSet<String>) -> String {
    let mut out = Vec::new();
    // The quote and name of a value that goes on over several lines.
    let mut open: Option<(char, String)> = None;
    for line in text.split('\n') {
        if let Some((quote, name)) = &open {
            let closed = closes(line, *quote);
            out.push(if shown.contains(name) {
                line.to_owned()
            } else {
                format!("[REDACTED:{name}]")
            });
            if closed {
                open = None;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push(line.to_owned());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            out.push("[REDACTED]".into());
            continue;
        };
        let name = key
            .trim()
            .strip_prefix("export")
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .unwrap_or(key.trim())
            .trim()
            .to_owned();
        let value = value.trim_start();
        if let Some(quote) = value.chars().next().filter(|c| *c == '"' || *c == '\'')
            && !closes(&value[1..], quote)
        {
            open = Some((quote, name.clone()));
        }
        out.push(if shown.contains(&name) {
            line.to_owned()
        } else {
            format!("{key}=[REDACTED:{name}]")
        });
    }
    out.join("\n")
}

/// Whether `text` has a closing `quote` that no backslash escapes.
fn closes(text: &str, quote: char) -> bool {
    let mut escaped = false;
    for c in text.chars() {
        match c {
            '\\' if !escaped => escaped = true,
            c if c == quote && !escaped => return true,
            _ => escaped = false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    use crate::tool::{Tool, ToolContext};
    use crate::workspace::Workspace;
    use crate::{EditTool, ReadTool, WriteTool};

    #[tokio::test]
    async fn file_tools_hide_and_keep_the_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Arc::new(Workspace::new(dir.path()).unwrap());
        let root = workspace.root().to_path_buf();
        let env_file = root.join(".env");
        std::fs::write(&env_file, "TOKEN=hidden-value\nREGION=eu\n").unwrap();
        let ctx = ToolContext::new(workspace).with_env(Arc::new(CommandEnv {
            protected: vec![env_file.clone()],
            shown: HashSet::from(["REGION".to_owned()]),
            ..Default::default()
        }));
        let read = ReadTool::default()
            .call(&ctx, json!({"path": ".env"}))
            .await
            .unwrap();
        assert_eq!(read.content, "TOKEN=[REDACTED:TOKEN]\nREGION=eu\n");
        let page = ReadTool::default()
            .call(&ctx, json!({"path": ".env", "offset": 2}))
            .await
            .unwrap();
        assert_eq!(page.content, "REGION=eu\n");

        let write = WriteTool
            .call(&ctx, json!({"path": ".env", "content": "X=1\n"}))
            .await
            .unwrap_err();
        assert!(write.to_string().contains("Ask the user"), "{write}");
        let patch =
            "--- a/.env\n+++ b/.env\n@@ -1,2 +1,2 @@\n-TOKEN=hidden-value\n+TOKEN=x\n REGION=eu\n";
        let edit = EditTool
            .call(&ctx, json!({"patch": patch}))
            .await
            .unwrap_err();
        assert!(edit.to_string().contains("Ask the user"), "{edit}");
        let moved = "--- a/other\n+++ b/.env\n@@ -1 +1 @@\n-a\n+b\n";
        std::fs::write(root.join("other"), "a\n").unwrap();
        assert!(EditTool.call(&ctx, json!({"patch": moved})).await.is_err());
        assert_eq!(
            std::fs::read_to_string(&env_file).unwrap(),
            "TOKEN=hidden-value\nREGION=eu\n"
        );

        // Other files are not touched.
        WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "TOKEN=x\n"}))
            .await
            .unwrap();
        let other = ReadTool::default()
            .call(&ctx, json!({"path": "a.txt"}))
            .await
            .unwrap();
        assert_eq!(other.content, "TOKEN=x\n");
    }

    fn redactor() -> Redactor {
        Redactor::new([
            ("API_TOKEN", "s3cr3t-t0ken-value"),
            ("SHORT", "abc"),
            ("PASS", "p@ss word/with:chars"),
        ])
    }

    #[test]
    fn values_and_their_encodings_are_redacted() {
        let r = redactor();
        assert_eq!(
            r.redact("token=s3cr3t-t0ken-value."),
            "token=[REDACTED:API_TOKEN]."
        );
        // Too short to redact.
        assert_eq!(r.redact("abc"), "abc");
        let encoded = STANDARD.encode("s3cr3t-t0ken-value");
        assert_eq!(r.redact(&encoded), "[REDACTED:API_TOKEN]");
        // Inside a longer base64 text, at any alignment.
        for prefix in ["", "a", "ab", "abc"] {
            let blob = STANDARD.encode(format!("{prefix}s3cr3t-t0ken-value and more"));
            let redacted = r.redact(&blob);
            assert!(
                redacted.contains("[REDACTED:API_TOKEN]"),
                "{blob} {redacted}"
            );
        }
        assert_eq!(
            r.redact("u=p%40ss%20word%2Fwith%3Achars"),
            "u=[REDACTED:PASS]"
        );
        assert_eq!(r.redact("nothing here"), Cow::Borrowed("nothing here"));
        assert!(Redactor::new([("A", "short")]).is_empty());
    }

    #[test]
    fn lines_of_a_value_with_several_lines_are_redacted() {
        let r = Redactor::new([("KEY", "first-line-1\r\nsecond-line-2")]);
        assert_eq!(r.redact("x first-line-1\ny"), "x [REDACTED:KEY]\ny");
        assert_eq!(r.redact("second-line-2"), "[REDACTED:KEY]");
    }

    #[test]
    fn a_value_split_over_chunks_is_still_redacted() {
        let r = redactor();
        let text = "a s3cr3t-t0ken-value b s3cr3t-t0ken-value c";
        for size in 1..text.len() {
            let mut stream = r.stream();
            let mut out = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                out.extend(stream.push(chunk));
            }
            out.extend(stream.finish());
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "a [REDACTED:API_TOKEN] b [REDACTED:API_TOKEN] c",
                "chunks of {size}"
            );
        }
        // Without secrets, chunks pass at once.
        let mut stream = Redactor::default().stream();
        assert_eq!(stream.push(b"abc"), b"abc");
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn dotenv_files_parse_and_report_the_line_of_an_error() {
        let vars = parse_dotenv("# c\nA=1\nexport B=\"two words\"\nC='x'\n").unwrap();
        assert_eq!(
            vars,
            [
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "x".into())
            ]
        );
        let err = parse_dotenv("A=1\nB C=hidden\n").unwrap_err();
        assert_eq!(err, "line 2 cannot be parsed");
        assert!(!err.contains("hidden"));
    }

    #[test]
    fn masked_dotenv_files_keep_names_comments_and_lines() {
        let text = "# AWS\nexport AWS_KEY=abc\nAWS_REGION=eu-west-1\n\nPEM=\"-----BEGIN\nsecret body\n-----END\"\nAFTER=1\njunk";
        let shown = HashSet::from(["AWS_REGION".to_owned()]);
        assert_eq!(
            mask_dotenv(text, &shown),
            "# AWS\nexport AWS_KEY=[REDACTED:AWS_KEY]\nAWS_REGION=eu-west-1\n\nPEM=[REDACTED:PEM]\n[REDACTED:PEM]\n[REDACTED:PEM]\nAFTER=[REDACTED:AFTER]\n[REDACTED]"
        );
    }
}
