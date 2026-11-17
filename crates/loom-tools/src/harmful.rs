use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};

pub type Check = Result<(), String>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    value: String,
    dynamic: bool,
    glob: bool,
}

impl Token {
    fn plain(value: String) -> Self {
        Self {
            value,
            dynamic: false,
            glob: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Separator {
    And,
    Or,
    Semicolon,
    Pipe,
    Background,
    Newline,
}

#[derive(Debug)]
struct Segment {
    command: String,
    before: Option<Separator>,
    after: Option<Separator>,
}

#[derive(Debug, Clone)]
struct Parsed {
    name: String,
    args: Vec<Token>,
}

type Scope = HashMap<String, Option<String>>;
type Assignments = BTreeMap<String, Option<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathPolicy {
    Delete,
    Modify,
    Write,
}

const GUARDED_COMMANDS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "mv", "cp", "chmod", "tee", "chown", "touch", "chgrp",
    "truncate", "dd", "ln",
];
const DELETING_COMMANDS: &[&str] = &["rm", "rmdir", "unlink", "shred"];
const TEMP_PATHS: &[&str] = &["/tmp", "/var/tmp", "/private/tmp", "/private/var/tmp"];
const DEVICE_PATHS: &[&str] = &["/dev/null", "/dev/stdin", "/dev/stdout", "/dev/stderr"];
const XARGS_COMMANDS: &[&str] = &["rm", "mv", "cp"];
const CONTROL_WORDS: &[&str] = &["!", "(", "{", "do", "else", "if", "then", "until", "while"];

fn wrapper_options(name: &str) -> Option<&'static [&'static str]> {
    Some(match name {
        "sudo" => &[
            "-C",
            "-D",
            "-g",
            "-h",
            "-p",
            "-R",
            "-r",
            "-T",
            "-t",
            "-u",
            "--chdir",
            "--close-from",
            "--group",
            "--host",
            "--other-user",
            "--prompt",
            "--role",
            "--type",
            "--user",
        ],
        "env" => &["-C", "-S", "-u", "--chdir", "--split-string", "--unset"],
        "command" | "nohup" => &[],
        "exec" => &["-a"],
        "nice" => &["-n", "--adjustment"],
        "time" => &["-f", "-o", "--format", "--output"],
        _ => return None,
    })
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn name_at(chars: &[char], start: usize) -> usize {
    if !chars.get(start).copied().is_some_and(is_name_start) {
        return 0;
    }
    chars[start..]
        .iter()
        .take_while(|c| is_name_char(**c))
        .count()
}

fn is_valid_name(name: &str) -> bool {
    let chars: Vec<char> = name.chars().collect();
    !chars.is_empty() && name_at(&chars, 0) == chars.len()
}

fn starts_with_assignment(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    let length = name_at(&chars, 0);
    length > 0 && chars.get(length) == Some(&'=')
}

fn parse_assignment(value: &str) -> Option<(String, bool, String)> {
    let chars: Vec<char> = value.chars().collect();
    let length = name_at(&chars, 0);
    if length == 0 {
        return None;
    }
    let name: String = chars[..length].iter().collect();
    let (append, equals) = match (chars.get(length), chars.get(length + 1)) {
        (Some('+'), Some('=')) => (true, length + 1),
        (Some('='), _) => (false, length),
        _ => return None,
    };
    Some((name, append, chars[equals + 1..].iter().collect()))
}

fn is_glob(c: char) -> bool {
    matches!(c, '*' | '?' | '[')
}

fn basename(value: &str) -> &str {
    value
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn split_components(value: &str) -> impl Iterator<Item = String> + '_ {
    value.split(['/', '\\']).map(str::to_owned)
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

fn find_heredocs(line: &[char], quotes: &mut (bool, bool)) -> Vec<(String, bool)> {
    let mut delimiters = Vec::new();
    let mut index = 0;
    while index < line.len() {
        let c = line[index];
        let next = line.get(index + 1).copied();
        let (single, double) = quotes;
        if c == '\\' && !*single {
            index += 2;
            continue;
        }
        if c == '\'' && !*double {
            *single = !*single;
            index += 1;
            continue;
        }
        if c == '"' && !*single {
            *double = !*double;
            index += 1;
            continue;
        }
        if *single || *double || c != '<' || next != Some('<') || line.get(index + 2) == Some(&'<')
        {
            index += 1;
            continue;
        }
        let mut cursor = index + 2;
        let strip_tabs = line.get(cursor) == Some(&'-');
        if strip_tabs {
            cursor += 1;
        }
        while line.get(cursor).is_some_and(|c| c.is_whitespace()) {
            cursor += 1;
        }
        let quote = line
            .get(cursor)
            .copied()
            .filter(|c| *c == '\'' || *c == '"');
        if quote.is_some() {
            cursor += 1;
        }
        let length = name_at(line, cursor);
        if length == 0 || quote.is_some_and(|q| line.get(cursor + length) != Some(&q)) {
            index += 1;
            continue;
        }
        delimiters.push((line[cursor..cursor + length].iter().collect(), strip_tabs));
        index = cursor + length + 1;
    }
    delimiters
}

fn strip_heredoc_bodies(command: &str) -> String {
    let mut delimiters: VecDeque<(String, bool)> = VecDeque::new();
    let mut quotes = (false, false);
    command
        .split('\n')
        .map(|line| {
            if let Some((delimiter, strip_tabs)) = delimiters.front() {
                let candidate = if *strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                if candidate.trim_end() == delimiter {
                    delimiters.pop_front();
                }
                return "";
            }
            let chars: Vec<char> = line.chars().collect();
            delimiters.extend(find_heredocs(&chars, &mut quotes));
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn split_command_chain(command: &str) -> Vec<Segment> {
    let source: Vec<char> = strip_heredoc_bodies(command).chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut before: Option<Separator> = None;
    let mut single = false;
    let mut double = false;

    let mut push = |current: &mut String, before: &mut Option<Separator>, after: Separator| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            segments.push(Segment {
                command: trimmed.to_owned(),
                before: *before,
                after: Some(after),
            });
        }
        current.clear();
        *before = Some(after);
    };

    let mut index = 0;
    while index < source.len() {
        let c = source[index];
        let next = source.get(index + 1).copied();
        if c == '\\' && !single {
            current.push(c);
            if let Some(next) = next {
                current.push(next);
                index += 1;
            }
            index += 1;
            continue;
        }
        if c == '\'' && !double {
            single = !single;
            current.push(c);
            index += 1;
            continue;
        }
        if c == '"' && !single {
            double = !double;
            current.push(c);
            index += 1;
            continue;
        }
        if single || double {
            current.push(c);
            index += 1;
            continue;
        }
        if c == '#' && (current.is_empty() || current.ends_with(char::is_whitespace)) {
            match source[index + 1..].iter().position(|c| *c == '\n') {
                Some(offset) => {
                    push(&mut current, &mut before, Separator::Newline);
                    index += offset + 2;
                    continue;
                }
                None => break,
            }
        }
        let previous = index.checked_sub(1).map(|i| source[i]);
        match (c, next) {
            ('&', Some('&')) => {
                push(&mut current, &mut before, Separator::And);
                index += 1;
            }
            ('&', next) if next != Some('>') && previous != Some('>') => {
                push(&mut current, &mut before, Separator::Background);
            }
            ('|', Some('|')) => {
                push(&mut current, &mut before, Separator::Or);
                index += 1;
            }
            ('|', next) => {
                push(&mut current, &mut before, Separator::Pipe);
                if next == Some('&') {
                    index += 1;
                }
            }
            (';', _) => push(&mut current, &mut before, Separator::Semicolon),
            ('\n', _) => push(&mut current, &mut before, Separator::Newline),
            _ => current.push(c),
        }
        index += 1;
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(Segment {
            command: trimmed.to_owned(),
            before,
            after: None,
        });
    }
    segments
}

struct Expansion {
    value: String,
    end: usize,
    dynamic: bool,
}

fn closing_paren(chars: &[char], open: usize) -> usize {
    let mut depth = 0;
    let mut single = false;
    let mut double = false;
    let mut index = open;
    while index < chars.len() {
        let c = chars[index];
        if c == '\\' && !single {
            index += 2;
            continue;
        }
        if c == '\'' && !double {
            single = !single;
        } else if c == '"' && !single {
            double = !double;
        } else if !single && !double {
            if c == '(' {
                depth += 1;
            }
            if c == ')' {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
        }
        index += 1;
    }
    chars.len()
}

fn unquoted_parens(command: &str) -> (i32, usize) {
    let mut balance = 0;
    let mut count = 0;
    let mut single = false;
    let mut double = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && !single {
            chars.next();
            continue;
        }
        if c == '\'' && !double {
            single = !single;
            continue;
        }
        if c == '"' && !single {
            double = !double;
            continue;
        }
        if single || double {
            continue;
        }
        if c == '(' {
            balance += 1;
            count += 1;
        }
        if c == ')' {
            balance -= 1;
            count += 1;
        }
    }
    (balance, count)
}

fn statement_assignments(tokens: &[Token]) -> Option<Assignments> {
    let start = usize::from(tokens.first().is_some_and(|t| t.value == "export"));
    let mut assignments = Assignments::new();
    for token in &tokens[start..] {
        let (name, append, value) = parse_assignment(&token.value)?;
        let resolvable = !append && !token.dynamic && !token.glob;
        assignments.insert(name, resolvable.then_some(value));
    }
    (!assignments.is_empty()).then_some(assignments)
}

fn merge_assignments(head: Option<Assignments>, other: Option<Assignments>) -> Option<Assignments> {
    let (head, other) = (head?, other?);
    let mut merged = Assignments::new();
    for name in head.keys().chain(other.keys()) {
        let value = head.get(name).cloned().flatten();
        let shared = head.contains_key(name)
            && other.contains_key(name)
            && other.get(name).cloned().flatten() == value;
        merged.insert(name.clone(), if shared { value } else { None });
    }
    Some(merged)
}

fn consumes_next(option: &str, options: &[&str]) -> bool {
    options.contains(&option) && !option.contains('=')
}

fn unwrap_command(tokens: &[Token]) -> Option<Parsed> {
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        let value = token.value.as_str();
        if CONTROL_WORDS.contains(&value) || value.is_empty() || starts_with_assignment(value) {
            index += 1;
            continue;
        }
        break;
    }
    while let Some(token) = tokens.get(index) {
        let name = basename(token.value.trim_start_matches(['(', '{']));
        let Some(options) = wrapper_options(name) else {
            return (!name.is_empty()).then(|| Parsed {
                name: name.to_owned(),
                args: tokens[index + 1..].to_vec(),
            });
        };
        let wrapper = name.to_owned();
        index += 1;
        while let Some(token) = tokens.get(index) {
            let option = token.value.as_str();
            if option == "--" {
                index += 1;
                break;
            }
            if wrapper == "env" && starts_with_assignment(option) {
                index += 1;
                continue;
            }
            if !option.starts_with('-') || option == "-" {
                break;
            }
            index += if consumes_next(option, options) { 2 } else { 1 };
        }
    }
    None
}

struct CopyMove {
    operands: Vec<Token>,
    target_directory: Option<Token>,
    no_target_directory: bool,
}

fn attached_target_option(value: &str) -> Option<usize> {
    let rest = value.strip_prefix('-')?;
    for (offset, c) in rest.char_indices() {
        match c {
            '-' => return None,
            't' => return Some(offset + 1),
            _ => {}
        }
    }
    None
}

fn parse_copy_move(args: &[Token]) -> CopyMove {
    let mut parsed = CopyMove {
        operands: Vec::new(),
        target_directory: None,
        no_target_directory: false,
    };
    let mut options = true;
    let mut index = 0;
    while let Some(token) = args.get(index) {
        let value = token.value.as_str();
        index += 1;
        if options && value == "--" {
            options = false;
            continue;
        }
        if options && (value == "-T" || value == "--no-target-directory") {
            parsed.no_target_directory = true;
            continue;
        }
        if options && (value == "-t" || value == "--target-directory") {
            parsed.target_directory = args.get(index).cloned();
            index += 1;
            continue;
        }
        if options && let Some(directory) = value.strip_prefix("--target-directory=") {
            parsed.target_directory = Some(Token {
                value: directory.to_owned(),
                ..token.clone()
            });
            continue;
        }
        if options && let Some(position) = attached_target_option(value) {
            let attached = &value[position + 1..];
            if attached.is_empty() {
                parsed.target_directory = args.get(index).cloned();
                index += 1;
            } else {
                parsed.target_directory = Some(Token {
                    value: attached.to_owned(),
                    ..token.clone()
                });
            }
            continue;
        }
        if options && (value == "-S" || value == "--suffix") {
            index += 1;
            continue;
        }
        if options && value.starts_with('-') {
            continue;
        }
        parsed.operands.push(token.clone());
    }
    parsed
}

fn simple_operands(args: &[Token]) -> Vec<Token> {
    let mut operands = Vec::new();
    let mut options = true;
    for token in args {
        if options && token.value == "--" {
            options = false;
            continue;
        }
        if options && token.value.starts_with('-') && token.value != "-" {
            continue;
        }
        operands.push(token.clone());
    }
    operands
}

fn mode_owner_targets(args: &[Token]) -> Vec<Token> {
    let mut targets = Vec::new();
    let mut metadata_seen = false;
    let mut options = true;
    let mut index = 0;
    while let Some(token) = args.get(index) {
        let value = token.value.as_str();
        index += 1;
        if options && value == "--" {
            options = false;
            continue;
        }
        if options && (value == "--reference" || value == "--from") {
            index += 1;
            continue;
        }
        if options && (value.starts_with("--reference=") || value.starts_with("--from=")) {
            continue;
        }
        if options && value.starts_with('-') && value != "-" {
            continue;
        }
        if metadata_seen {
            targets.push(token.clone());
        } else {
            metadata_seen = true;
        }
    }
    targets
}

fn targets_with_valued_options(
    args: &[Token],
    separate: &[&str],
    joined: &[&str],
    short: &[char],
) -> Vec<Token> {
    let mut targets = Vec::new();
    let mut options = true;
    let mut index = 0;
    while let Some(token) = args.get(index) {
        let value = token.value.as_str();
        index += 1;
        if options && value == "--" {
            options = false;
            continue;
        }
        if options && separate.contains(&value) {
            index += 1;
            continue;
        }
        if options && joined.iter().any(|prefix| value.starts_with(prefix)) {
            continue;
        }
        let mut chars = value.chars();
        if options
            && chars.next() == Some('-')
            && chars.next().is_some_and(|c| short.contains(&c))
            && chars.next().is_some()
        {
            continue;
        }
        if options && value.starts_with('-') && value != "-" {
            continue;
        }
        targets.push(token.clone());
    }
    targets
}

fn touch_targets(args: &[Token]) -> Vec<Token> {
    targets_with_valued_options(
        args,
        &["-d", "-r", "-t", "--date", "--reference", "--time"],
        &["--date=", "--reference=", "--time="],
        &['d', 'r', 't'],
    )
}

fn truncate_targets(args: &[Token]) -> Vec<Token> {
    targets_with_valued_options(
        args,
        &["-r", "-s", "--reference", "--size"],
        &["--reference=", "--size="],
        &['r', 's'],
    )
}

fn dd_output(args: &[Token]) -> Option<Token> {
    args.iter().find_map(|token| {
        token.value.strip_prefix("of=").map(|value| Token {
            value: value.to_owned(),
            ..token.clone()
        })
    })
}

fn compound_candidates(tokens: &[Token]) -> Vec<Token> {
    tokens
        .iter()
        .filter(|token| {
            let value = token.value.as_str();
            !value.is_empty()
                && !value.starts_with('-')
                && !["+", ";", "{}"].contains(&value)
                && !XARGS_COMMANDS.contains(&basename(value))
        })
        .cloned()
        .collect()
}

fn git_subcommand(args: &[Token]) -> Option<(String, &[Token])> {
    const VALUED: &[&str] = &[
        "-C",
        "-c",
        "--exec-path",
        "--git-dir",
        "--namespace",
        "--work-tree",
    ];
    let mut index = 0;
    while let Some(token) = args.get(index) {
        let value = token.value.as_str();
        if value == "--" {
            return args
                .get(index + 1)
                .map(|name| (name.value.clone(), &args[index + 2..]));
        }
        if consumes_next(value, VALUED) {
            index += 2;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((value.to_owned(), &args[index + 1..]));
    }
    None
}

fn check_git(command: &Parsed) -> Check {
    if command.name != "git" {
        return Ok(());
    }
    let Some((name, args)) = git_subcommand(&command.args) else {
        return Ok(());
    };
    let options: Vec<&str> = args.iter().map(|token| token.value.as_str()).collect();
    let has = |option: &str| options.contains(&option);
    let has_short = |flag: char| {
        options.iter().any(|option| {
            option.len() > 1
                && option.starts_with('-')
                && !option[1..].contains('-')
                && option[1..].contains(flag)
        })
    };
    let blocked = |what: &str| Err(format!("Dangerous git command blocked: \"git {what}\"."));
    match name.as_str() {
        "checkout" if has("--") => blocked("checkout --"),
        "restore" if !has("--staged") || has("--worktree") => blocked("restore"),
        "reset"
            if options
                .iter()
                .any(|o| *o == "--hard" || o.starts_with("--hard=")) =>
        {
            blocked("reset --hard")
        }
        "reset" if has("--merge") => blocked("reset --merge"),
        "push" if options.iter().any(|o| o.starts_with("--force")) || has_short('f') => {
            blocked("push --force")
        }
        "clean" if has("--force") || has_short('f') => blocked("clean -f"),
        "branch" if has("-D") => blocked("branch -D"),
        "stash" if matches!(options.first(), Some(&"drop") | Some(&"clear")) => {
            blocked(&format!("stash {}", options[0]))
        }
        _ => Ok(()),
    }
}

pub struct CommandAnalyzer {
    root: PathBuf,
    home: PathBuf,
    allowed: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    env: HashMap<String, String>,
}

impl CommandAnalyzer {
    pub fn new(
        root: &Path,
        home: PathBuf,
        allow_paths: &[String],
        deny_paths: &[String],
        env: HashMap<String, String>,
    ) -> Self {
        let mut analyzer = Self {
            root: PathBuf::new(),
            home,
            allowed: Vec::new(),
            denied: Vec::new(),
            env,
        };
        analyzer.root = analyzer.resolve_symlinks(&root.to_string_lossy(), root);
        analyzer.allowed = analyzer.configured_roots(allow_paths);
        analyzer.denied = analyzer.configured_roots(deny_paths);
        analyzer
    }

    fn configured_roots(&self, paths: &[String]) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for entry in paths {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            for root in [
                normalize(&self.root.join(self.expand_home(entry))),
                self.resolve_symlinks(entry, &self.root),
            ] {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
        }
        roots
    }

    fn expand_home(&self, input: &str) -> String {
        match input.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
                format!("{}{rest}", self.home.display())
            }
            _ => input.to_owned(),
        }
    }

    fn resolve_symlinks(&self, input: &str, base: &Path) -> PathBuf {
        let expanded = self.expand_home(input);
        let (mut current, mut pending): (PathBuf, VecDeque<String>) =
            if let Some(rest) = expanded.strip_prefix('/') {
                (PathBuf::from("/"), split_components(rest).collect())
            } else {
                (
                    base.canonicalize().unwrap_or_else(|_| normalize(base)),
                    split_components(&expanded).collect(),
                )
            };
        let mut followed = 0;
        while let Some(component) = pending.pop_front() {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." {
                current.pop();
                continue;
            }
            let candidate = current.join(&component);
            let is_link = std::fs::symlink_metadata(&candidate)
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            if !is_link {
                current = candidate;
                continue;
            }
            followed += 1;
            if followed > 40 {
                return candidate;
            }
            let Ok(target) = std::fs::read_link(&candidate) else {
                current = candidate;
                continue;
            };
            let target = target.to_string_lossy().into_owned();
            let parts: Vec<String> = match target.strip_prefix('/') {
                Some(rest) => {
                    current = PathBuf::from("/");
                    split_components(rest).collect()
                }
                None => split_components(&target).collect(),
            };
            for part in parts.into_iter().rev() {
                pending.push_front(part);
            }
        }
        normalize(&current)
    }

    fn is_inside(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    fn matches_allowed(&self, candidates: &[PathBuf]) -> bool {
        self.allowed.iter().any(|root| {
            candidates
                .iter()
                .any(|candidate| candidate.starts_with(root))
        })
    }

    fn matches_denied(&self, candidates: &[PathBuf], token: &Token) -> bool {
        self.denied.iter().any(|root| {
            candidates.iter().any(|candidate| {
                if candidate.starts_with(root) || root.starts_with(candidate) {
                    return true;
                }
                if !token.glob {
                    return false;
                }
                let candidate = candidate.to_string_lossy();
                candidate.find(is_glob).is_some_and(|wildcard| {
                    root.to_string_lossy().starts_with(&candidate[..wildcard])
                })
            })
        })
    }

    fn is_allowed_outside(&self, resolved: &Path, policy: PathPolicy) -> bool {
        let under = |paths: &[&str]| {
            paths
                .iter()
                .any(|path| resolved.starts_with(self.resolve_symlinks(path, &self.root)))
        };
        let tmpdir = self
            .env
            .get("TMPDIR")
            .filter(|dir| dir.starts_with('/') && !dir.trim_matches('/').is_empty())
            .map(String::as_str);
        under(TEMP_PATHS)
            || tmpdir.is_some_and(|dir| under(&[dir]))
            || (policy != PathPolicy::Delete && under(DEVICE_PATHS))
    }

    fn protected_name(&self, path: &Path, token: &Token) -> Option<&'static str> {
        let relative = path.strip_prefix(&self.root).ok()?.to_string_lossy();
        let relative = relative.as_ref();
        let is_env = relative == ".env"
            || relative
                .strip_prefix(".env.")
                .is_some_and(|rest| !rest.is_empty() && rest != "example");
        if is_env {
            return Some(".env files");
        }
        if relative == ".git" || relative.starts_with(".git/") {
            return Some(".git directory");
        }
        let first = relative.split('/').next().unwrap_or_default();
        if token.glob
            && first.starts_with('.')
            && let Some(wildcard) = first.find(is_glob)
        {
            let prefix = &first[..wildcard];
            if prefix.starts_with(".env") || ".env".starts_with(prefix) {
                return Some(".env files");
            }
            if prefix.starts_with(".git") || ".git".starts_with(prefix) {
                return Some(".git directory");
            }
        }
        None
    }

    fn check_target(&self, command: &str, token: &Token, cwd: &Path, policy: PathPolicy) -> Check {
        if token.dynamic {
            return Err(format!(
                "Command \"{command}\" has a target path that cannot be safely resolved."
            ));
        }
        if token.value.is_empty() || token.value == "-" {
            return Ok(());
        }
        let lexical = normalize(&cwd.join(self.expand_home(&token.value)));
        let resolved = self.resolve_symlinks(&token.value, cwd);
        let candidates = [lexical.clone(), resolved.clone()];
        if self.matches_denied(&candidates, token) {
            return Err(format!(
                "Command \"{command}\" targets a denied path: {}",
                token.value
            ));
        }
        if self.matches_allowed(&candidates) {
            return Ok(());
        }
        let protected = |name: &str| {
            Err(format!(
                "Command \"{command}\" targets protected path {name}: {}",
                token.value
            ))
        };
        if self.is_inside(&lexical)
            && let Some(name) = self.protected_name(&lexical, token)
        {
            return protected(name);
        }
        if !self.is_inside(&resolved) {
            if self.is_allowed_outside(&resolved, policy) {
                return Ok(());
            }
            return Err(format!(
                "Command \"{command}\" targets a path outside the working directory: {}",
                token.value
            ));
        }
        match self.protected_name(&resolved, token) {
            Some(name) => protected(name),
            None => Ok(()),
        }
    }

    fn check_targets(
        &self,
        command: &str,
        targets: &[Token],
        cwd: &Path,
        policy: PathPolicy,
    ) -> Check {
        targets
            .iter()
            .try_for_each(|target| self.check_target(command, target, cwd, policy))
    }

    fn tokens(&self, command: &str, cwd: &Path, scope: &Scope) -> Vec<Token> {
        let chars: Vec<char> = command.chars().collect();
        let mut tokens = Vec::new();
        let mut value = String::new();
        let mut started = false;
        let mut dynamic = false;
        let mut glob = false;
        let mut single = false;
        let mut double = false;

        let mut push =
            |value: &mut String, started: &mut bool, dynamic: &mut bool, glob: &mut bool| {
                if *started {
                    tokens.push(Token {
                        value: std::mem::take(value),
                        dynamic: *dynamic,
                        glob: *glob,
                    });
                }
                value.clear();
                *started = false;
                *dynamic = false;
                *glob = false;
            };

        let mut index = 0;
        while index < chars.len() {
            let c = chars[index];
            let next = chars.get(index + 1).copied();
            if c == '\\' && !single {
                if next == Some('\n') {
                    index += 2;
                    continue;
                }
                started = true;
                if let Some(next) = next {
                    value.push(next);
                    index += 1;
                }
                index += 1;
                continue;
            }
            if c == '\'' && !double {
                single = !single;
                started = true;
                index += 1;
                continue;
            }
            if c == '"' && !single {
                double = !double;
                started = true;
                index += 1;
                continue;
            }
            if !single && c == '`' {
                dynamic = true;
                started = true;
                index += 1;
                continue;
            }
            if !single && c == '$' {
                let expansion = self.expand_variable(&chars, index, cwd, scope);
                value.push_str(&expansion.value);
                dynamic |= expansion.dynamic;
                started = true;
                index = expansion.end + 1;
                continue;
            }
            if !single && (c == '<' || c == '>') && next == Some('(') {
                let end = closing_paren(&chars, index + 1);
                value.extend(&chars[index..(end + 1).min(chars.len())]);
                dynamic = true;
                started = true;
                index = end + 1;
                continue;
            }
            if !single && !double && (c == '(' || c == ')') {
                push(&mut value, &mut started, &mut dynamic, &mut glob);
                index += 1;
                continue;
            }
            if !single && !double && is_glob(c) {
                glob = true;
            }
            if !single && !double && c.is_whitespace() {
                push(&mut value, &mut started, &mut dynamic, &mut glob);
                index += 1;
                continue;
            }
            value.push(c);
            started = true;
            index += 1;
        }
        push(&mut value, &mut started, &mut dynamic, &mut glob);
        tokens
    }

    fn lookup(&self, name: &str, cwd: &Path, scope: &Scope) -> (String, bool) {
        if let Some(assigned) = scope.get(name) {
            return match assigned {
                Some(value) => (value.clone(), false),
                None => (String::new(), true),
            };
        }
        if name == "PWD" {
            return (cwd.to_string_lossy().into_owned(), false);
        }
        match self.env.get(name) {
            Some(value) => (value.clone(), false),
            None => (String::new(), true),
        }
    }

    fn expand_variable(
        &self,
        chars: &[char],
        index: usize,
        cwd: &Path,
        scope: &Scope,
    ) -> Expansion {
        let next = chars.get(index + 1).copied();
        let dynamic = |end| Expansion {
            value: String::new(),
            end,
            dynamic: true,
        };
        match next {
            Some('(') => return dynamic(index + 1),
            Some('\'') | Some('"') => return dynamic(index),
            Some('{') => {
                let Some(offset) = chars[index + 2..].iter().position(|c| *c == '}') else {
                    return dynamic(index);
                };
                let end = index + 2 + offset;
                let name: String = chars[index + 2..end].iter().collect();
                if !is_valid_name(&name) {
                    return dynamic(end);
                }
                let (value, dynamic) = self.lookup(&name, cwd, scope);
                return Expansion {
                    value,
                    end,
                    dynamic,
                };
            }
            _ => {}
        }
        let length = name_at(chars, index + 1);
        if length == 0 {
            return Expansion {
                value: "$".into(),
                end: index,
                dynamic: next.is_some_and(|c| c.is_ascii_digit() || "@*#?$!-".contains(c)),
            };
        }
        let name: String = chars[index + 1..index + 1 + length].iter().collect();
        let (value, dynamic) = self.lookup(&name, cwd, scope);
        Expansion {
            value,
            end: index + length,
            dynamic,
        }
    }

    fn check_redirects(&self, command: &str, cwd: &Path, scope: &Scope) -> Check {
        let chars: Vec<char> = command.chars().collect();
        let mut single = false;
        let mut double = false;
        let mut index = 0;
        while index < chars.len() {
            let c = chars[index];
            if c == '\\' && !single {
                index += 2;
                continue;
            }
            if c == '\'' && !double {
                single = !single;
                index += 1;
                continue;
            }
            if c == '"' && !single {
                double = !double;
                index += 1;
                continue;
            }
            index += 1;
            if single || double || c != '>' {
                continue;
            }
            let mut cursor = index + usize::from(chars.get(index) == Some(&'>'));
            if chars.get(cursor) == Some(&'|') {
                cursor += 1;
            }
            let skip_spaces = |mut cursor: usize| {
                while chars.get(cursor).is_some_and(|c| c.is_whitespace()) {
                    cursor += 1;
                }
                cursor
            };
            cursor = skip_spaces(cursor);
            if chars.get(cursor) == Some(&'&') {
                cursor = skip_spaces(cursor + 1);
                if chars
                    .get(cursor)
                    .is_some_and(|c| c.is_ascii_digit() || *c == '-')
                {
                    continue;
                }
            }
            let rest: String = chars[cursor.min(chars.len())..].iter().collect();
            if let Some(target) = self.tokens(&rest, cwd, scope).first() {
                self.check_target("redirect", target, cwd, PathPolicy::Write)?;
            }
        }
        Ok(())
    }

    fn check_compound(&self, command: &Parsed, all_args: &[Token], cwd: &Path) -> Check {
        let values: Vec<&str> = command.args.iter().map(|t| t.value.as_str()).collect();
        let runs_xargs_command = values.iter().any(|v| XARGS_COMMANDS.contains(&basename(v)));
        let mut name = None;
        let mut candidates = command.args.as_slice();
        if command.name == "find" && values.contains(&"-delete") {
            name = Some("find -delete");
        }
        if command.name == "find" && values.contains(&"-exec") && runs_xargs_command {
            name = Some("find -exec");
        }
        if command.name == "xargs" && runs_xargs_command {
            name = Some("xargs");
            candidates = all_args;
        }
        if command.name == "rsync" && values.contains(&"--delete") {
            name = Some("rsync --delete");
        }
        match name {
            Some(name) => self.check_targets(
                name,
                &compound_candidates(candidates),
                cwd,
                PathPolicy::Delete,
            ),
            None => Ok(()),
        }
    }

    fn is_directory(&self, token: &Token, cwd: &Path, sources: usize) -> bool {
        sources > 1
            || token.value.ends_with(['/', '\\'])
            || self.resolve_symlinks(&token.value, cwd).is_dir()
    }

    fn check_copy_move(&self, command: &str, args: &[Token], cwd: &Path) -> Check {
        let parsed = parse_copy_move(args);
        let (sources, destination) = match parsed.target_directory {
            Some(directory) => (parsed.operands, directory),
            None => {
                let mut operands = parsed.operands;
                if operands.len() < 2 {
                    return Ok(());
                }
                let destination = operands.pop().expect("two operands");
                (operands, destination)
            }
        };
        if command == "mv" {
            self.check_targets(command, &sources, cwd, PathPolicy::Delete)?;
        }
        self.check_target(command, &destination, cwd, PathPolicy::Write)?;
        if !parsed.no_target_directory && self.is_directory(&destination, cwd, sources.len()) {
            for source in &sources {
                if source.dynamic {
                    return Err(format!(
                        "Command \"{command}\" has a target path that cannot be safely resolved."
                    ));
                }
                let derived = Token {
                    value: format!(
                        "{}/{}",
                        destination.value.trim_end_matches('/'),
                        basename(&source.value)
                    ),
                    dynamic: false,
                    glob: source.glob || destination.glob,
                };
                self.check_target(command, &derived, cwd, PathPolicy::Write)?;
            }
        }
        Ok(())
    }

    fn check_guarded(&self, command: &Parsed, cwd: &Path) -> Check {
        let name = command.name.as_str();
        if !GUARDED_COMMANDS.contains(&name) {
            return Ok(());
        }
        let args = &command.args;
        match name {
            "cp" | "mv" => self.check_copy_move(name, args, cwd),
            "chmod" | "chown" | "chgrp" => {
                self.check_targets(name, &mode_owner_targets(args), cwd, PathPolicy::Modify)
            }
            "touch" => self.check_targets(name, &touch_targets(args), cwd, PathPolicy::Modify),
            "truncate" => self.check_targets(name, &truncate_targets(args), cwd, PathPolicy::Write),
            "dd" => match dd_output(args) {
                Some(output) => self.check_target(name, &output, cwd, PathPolicy::Write),
                None => Ok(()),
            },
            _ => {
                let policy = if name == "tee" {
                    PathPolicy::Write
                } else if DELETING_COMMANDS.contains(&name) {
                    PathPolicy::Delete
                } else {
                    PathPolicy::Modify
                };
                self.check_targets(name, &simple_operands(args), cwd, policy)
            }
        }
    }

    fn cd_target(&self, command: &Parsed, cwd: &Path) -> Option<PathBuf> {
        if command.name != "cd" {
            return None;
        }
        match simple_operands(&command.args).first() {
            None => Some(self.resolve_symlinks(&self.home.to_string_lossy(), cwd)),
            Some(target) if target.dynamic || target.value == "-" => None,
            Some(target) => Some(self.resolve_symlinks(&target.value, cwd)),
        }
    }

    pub fn analyze(&self, command: &str) -> Check {
        let segments = split_command_chain(command);
        let mut directories = vec![self.root.clone()];
        let mut variables = Scope::new();
        let mut depth = 0;
        for segment in &segments {
            let pipeline =
                segment.before == Some(Separator::Pipe) || segment.after == Some(Separator::Pipe);
            let (balance, count) = unquoted_parens(&segment.command);
            let plain = depth == 0 && count == 0;
            depth += balance;
            let parsed: Vec<(PathBuf, Vec<Token>, Option<Parsed>)> = directories
                .iter()
                .map(|cwd| {
                    let tokens = self.tokens(&segment.command, cwd, &variables);
                    let command = unwrap_command(&tokens);
                    (cwd.clone(), tokens, command)
                })
                .collect();

            for (cwd, _, command) in &parsed {
                self.check_redirects(&segment.command, cwd, &variables)?;
                let Some(command) = command else {
                    continue;
                };
                check_git(command)?;
                self.check_guarded(command, cwd)?;
                let all_args: Vec<Token> = segments
                    .iter()
                    .flat_map(|candidate| {
                        unwrap_command(&self.tokens(&candidate.command, cwd, &variables))
                            .map(|parsed| parsed.args)
                            .unwrap_or_default()
                    })
                    .collect();
                self.check_compound(command, &all_args, cwd)?;
            }

            if pipeline {
                continue;
            }
            if plain && segment.after != Some(Separator::Background) {
                let assignments = parsed
                    .iter()
                    .map(|(_, tokens, _)| statement_assignments(tokens))
                    .reduce(merge_assignments)
                    .flatten();
                if let Some(assignments) = assignments {
                    variables.extend(assignments);
                }
            }
            let mut changed: Vec<PathBuf> = Vec::new();
            for (cwd, _, command) in &parsed {
                if let Some(target) = command.as_ref().and_then(|c| self.cd_target(c, cwd))
                    && !changed.contains(&target)
                {
                    changed.push(target);
                }
            }
            if changed.is_empty() {
                continue;
            }
            if matches!(segment.before, Some(Separator::And) | Some(Separator::Or)) {
                for directory in changed {
                    if !directories.contains(&directory) {
                        directories.push(directory);
                    }
                }
            } else {
                directories = changed;
            }
        }
        Ok(())
    }

    pub fn validate_path(&self, path: &str) -> Check {
        if path.is_empty() {
            return Ok(());
        }
        self.check_target(
            "file operation",
            &Token::plain(path.to_owned()),
            &self.root,
            PathPolicy::Write,
        )
    }
}
