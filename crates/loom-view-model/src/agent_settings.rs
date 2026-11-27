use std::str::FromStr;

use loom_config::{AgentConfig, ByteSize, ConfigDocument, HumanDuration, PolicyAction, PolicyRule};
use serde_json::{Map, Value};

pub const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];
pub const POLICY_ACTIONS: &[&str] = &["allow", "ask", "deny", "allow_harmful"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Multiline,
    Integer,
    Number,
    Size,
    Duration,
    Check,
    Choice(&'static [&'static str]),
    OptionalChoice(&'static [&'static str]),
    List,
    Map,
    Accounts,
    Credentials,
    Secrets,
    Rules,
}

impl Kind {
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Kind::Text
                | Kind::Multiline
                | Kind::Integer
                | Kind::Number
                | Kind::Size
                | Kind::Duration
                | Kind::List
                | Kind::Map
        )
    }

    pub fn is_lines(self) -> bool {
        matches!(self, Kind::Multiline | Kind::List | Kind::Map)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub path: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub placeholder: &'static str,
    pub kind: Kind,
}

const fn field(path: &'static str, label: &'static str, kind: Kind, help: &'static str) -> Field {
    Field {
        path,
        label,
        help,
        placeholder: "",
        kind,
    }
}

const fn hint(
    path: &'static str,
    label: &'static str,
    kind: Kind,
    placeholder: &'static str,
    help: &'static str,
) -> Field {
    Field {
        path,
        label,
        help,
        placeholder,
        kind,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    pub help: &'static str,
    pub fields: &'static [Field],
}

pub const YAML_ONLY: &[&str] = &["version", "llm.providers", "relay.cloudflare_access"];

pub const SECTIONS: &[Section] = &[
    Section {
        id: "model",
        title: "Model",
        help: "The model of new sessions and the model accounts of the oracle. Set custom providers in llm.providers in the YAML view.",
        fields: &[
            hint(
                "llm.default_model",
                "Default model",
                Kind::Text,
                "anthropic/claude-sonnet-4-5",
                "The model of new sessions, in the form <provider>/<model>.",
            ),
            field(
                "llm.default_reasoning_effort",
                "Reasoning effort",
                Kind::Choice(EFFORTS),
                "The reasoning effort of new sessions.",
            ),
            field(
                "llm.accounts",
                "Model accounts",
                Kind::Accounts,
                "The model account of each provider. A codespace can use only the providers that have an account.",
            ),
        ],
    },
    Section {
        id: "policy",
        title: "Policy",
        help: "Tells if the agent runs a tool call, refuses it, or asks for your approval.",
        fields: &[
            field(
                "policy.default",
                "Default action",
                Kind::Choice(POLICY_ACTIONS),
                "The action of new sessions for the calls that no rule matches. In a preset, the default is allow, because the codespace is the sandbox.",
            ),
            field(
                "policy.rules",
                "Rules",
                Kind::Rules,
                "Each rule matches a tool, and for the shell tool, optional command prefixes.",
            ),
            hint(
                "policy.harmful.deny_paths",
                "Denied paths",
                Kind::List,
                "./infra/production",
                "Changes to these paths are always harmful. One path on each line.",
            ),
        ],
    },
    Section {
        id: "instructions",
        title: "Instructions",
        help: "The instructions and the skills of the model. The agent also adds the AGENTS.md files.",
        fields: &[
            hint(
                "instructions.system_prompt",
                "System prompt",
                Kind::Multiline,
                "You are a careful coding agent.",
                "Replaces the built-in instructions of Loom. The agent still adds the AGENTS.md files and the runtime section. Empty for the built-in instructions.",
            ),
            field(
                "instructions.max_bytes",
                "Maximum size of instructions",
                Kind::Size,
                "The maximum size of the AGENTS.md files, such as 64KiB.",
            ),
            field(
                "skills.enabled",
                "Offer skills to the model",
                Kind::Check,
                "",
            ),
            hint(
                "skills.paths",
                "Skill directories",
                Kind::List,
                "~/shared-skills",
                "More directories of skills. A relative path is relative to the workspace. One path on each line.",
            ),
            hint(
                "skills.disabled",
                "Disabled skills",
                Kind::List,
                "slow-review",
                "The names of the skills not to offer. One name on each line.",
            ),
        ],
    },
    Section {
        id: "tools",
        title: "Tools",
        help: "The limits of the read and shell tools, the notes, and the task list.",
        fields: &[
            hint(
                "tools.allowed_dirs",
                "Allowed directories",
                Kind::List,
                "~/notes",
                "Directories outside the workspace for the read, edit, and write tools. Each path must be absolute or start with ~. One path on each line.",
            ),
            field(
                "tools.read.max_lines",
                "Read: maximum lines",
                Kind::Integer,
                "",
            ),
            field("tools.read.max_bytes", "Read: maximum size", Kind::Size, ""),
            field(
                "tools.shell.program",
                "Shell: program",
                Kind::Text,
                "The shell program of the shell tool.",
            ),
            hint(
                "tools.shell.timeout",
                "Shell: timeout",
                Kind::Duration,
                "10m",
                "Empty for no timeout.",
            ),
            field(
                "tools.shell.max_lines",
                "Shell: maximum output lines",
                Kind::Integer,
                "",
            ),
            field(
                "tools.shell.max_bytes",
                "Shell: maximum output size",
                Kind::Size,
                "",
            ),
            field(
                "tools.shell.pty",
                "Shell: run commands in a pseudo-terminal",
                Kind::Check,
                "",
            ),
            field(
                "tools.notes.max_bytes",
                "Notes: maximum size",
                Kind::Size,
                "",
            ),
            field(
                "tools.tasks.max_items",
                "Tasks: maximum tasks",
                Kind::Integer,
                "",
            ),
        ],
    },
    Section {
        id: "web_search",
        title: "Web search",
        help: "The search_web tool uses an OpenAI model to search the web. It needs an openai-codex account.",
        fields: &[
            field("tools.web_search.model", "Model", Kind::Text, ""),
            field(
                "tools.web_search.max_sources",
                "Sources",
                Kind::Integer,
                "The number of sources when the model does not give a number.",
            ),
            field(
                "tools.web_search.max_allowed_sources",
                "Maximum sources",
                Kind::Integer,
                "The maximum number of sources that the model can ask for.",
            ),
            field("tools.web_search.timeout", "Timeout", Kind::Duration, ""),
            hint(
                "tools.web_search.endpoint",
                "Endpoint",
                Kind::Text,
                "https://chatgpt.com/backend-api/codex/responses",
                "The Responses endpoint of the ChatGPT backend. Empty for the default.",
            ),
            field(
                "tools.web_search.reasoning_effort",
                "Reasoning effort",
                Kind::OptionalChoice(EFFORTS),
                "Default lets the model select.",
            ),
        ],
    },
    Section {
        id: "comment_guard",
        title: "Comment guard",
        help: "Stops the model when it adds comments to code.",
        fields: &[
            field(
                "tools.comment_guard.mode",
                "Mode",
                Kind::Choice(&["off", "warn", "block"]),
                "block rejects a change with new comments. warn makes the change and tells the model. off does not examine comments.",
            ),
            field(
                "tools.comment_guard.allow_directives",
                "Allow tool directives, such as eslint-disable",
                Kind::Check,
                "",
            ),
            hint(
                "tools.comment_guard.allow_patterns",
                "Allowed patterns",
                Kind::List,
                "TODO\\(#\\d+\\)",
                "Regular expressions of the comments to allow. One pattern on each line.",
            ),
            hint(
                "tools.comment_guard.ignore_paths",
                "Ignored paths",
                Kind::List,
                "vendor",
                "Paths in the workspace not to examine. One path on each line.",
            ),
        ],
    },
    Section {
        id: "environment",
        title: "Environment",
        help: "The environment variables of shell commands and terminals.",
        fields: &[
            field(
                "environment.inherit",
                "Inherit",
                Kind::Choice(&["all", "core", "none"]),
                "The variables of the agent that commands get. core is HOME, LOGNAME, PATH, SHELL, USER, USERNAME, the temporary directories, the locale, and TZ.",
            ),
            field(
                "environment.ignore_default_excludes",
                "Keep the variables with KEY, SECRET, TOKEN, or PASSWORD in the name",
                Kind::Check,
                "",
            ),
            hint(
                "environment.exclude",
                "Exclude",
                Kind::List,
                "AWS_*",
                "Name patterns of the variables to remove. Use * and ?. One pattern on each line.",
            ),
            hint(
                "environment.include_only",
                "Include only",
                Kind::List,
                "PATH",
                "Name patterns of the only variables that commands get. Empty keeps all. One pattern on each line.",
            ),
            hint(
                "environment.set",
                "Variables",
                Kind::Map,
                "RUST_BACKTRACE=1",
                "Variables to set, as NAME=value. One variable on each line.",
            ),
            field(
                "environment.secrets",
                "Secrets",
                Kind::Secrets,
                "The secret of the oracle for each variable. The model sees only the names.",
            ),
            field(
                "environment.redaction.enabled",
                "Hide the values of secret variables from the model",
                Kind::Check,
                "",
            ),
            hint(
                "environment.redaction.allow",
                "Values to show",
                Kind::List,
                "AWS_REGION",
                "Names of the variables whose values are not secret. One name on each line.",
            ),
        ],
    },
    Section {
        id: "git",
        title: "Git",
        help: "For each repository, the codespace uses the credential with the smallest scope that matches. If two credentials have the same scope, the first in the list wins.",
        fields: &[field(
            "git.credentials",
            "Git credentials",
            Kind::Credentials,
            "",
        )],
    },
    Section {
        id: "sandbox",
        title: "Sandbox",
        help: "Confines shell commands. Changes in the writable paths are not harmful, also when the sandbox is off.",
        fields: &[
            field(
                "sandbox.backend",
                "Backend",
                Kind::Choice(&["none", "landlock", "bubblewrap"]),
                "",
            ),
            field("sandbox.network", "Allow the network", Kind::Check, ""),
            hint(
                "sandbox.writable",
                "Writable paths",
                Kind::List,
                "~/.cache",
                "One path on each line.",
            ),
            field("sandbox.max_output", "Maximum output", Kind::Size, ""),
        ],
    },
    Section {
        id: "context",
        title: "Context and limits",
        help: "The limits of tool output, when to compact a long conversation, and the limit of one turn.",
        fields: &[
            field(
                "context.max_tool_output_tokens",
                "Maximum tool output tokens",
                Kind::Integer,
                "",
            ),
            hint(
                "context.window_tokens",
                "Context window tokens",
                Kind::Integer,
                "200000",
                "Empty for the window of the model.",
            ),
            field(
                "context.compaction_threshold",
                "Compaction threshold",
                Kind::Number,
                "The part of the context window at which the agent compacts the conversation.",
            ),
            field(
                "limits.max_rounds_per_turn",
                "Maximum model calls in one turn",
                Kind::Integer,
                "0 sets no limit.",
            ),
        ],
    },
    Section {
        id: "terminal",
        title: "Terminal and logs",
        help: "The terminals of the codespace and the log level of the agent.",
        fields: &[
            hint(
                "terminal.shell",
                "Terminal shell",
                Kind::Text,
                "/bin/bash",
                "Empty for the shell of the user.",
            ),
            field(
                "terminal.detached_timeout",
                "Detached timeout",
                Kind::Duration,
                "How long a terminal stays open without a client. 0s closes it when the client disconnects.",
            ),
            field(
                "logging.level",
                "Log level",
                Kind::Choice(&["error", "warn", "info", "debug", "trace"]),
                "",
            ),
        ],
    },
    Section {
        id: "telemetry",
        title: "Telemetry",
        help: "OpenTelemetry (OTLP) export of metrics and events. Changes apply after the agent restarts.",
        fields: &[
            field("telemetry.enabled", "Export telemetry", Kind::Check, ""),
            field(
                "telemetry.protocol",
                "Protocol",
                Kind::OptionalChoice(&["grpc", "http/protobuf", "http/json"]),
                "Default uses OTEL_EXPORTER_OTLP_PROTOCOL, or http/protobuf.",
            ),
            hint(
                "telemetry.endpoint",
                "Endpoint",
                Kind::Text,
                "http://localhost:4318",
                "The base URL of the collector.",
            ),
            hint(
                "telemetry.headers",
                "Headers",
                Kind::Map,
                "authorization=Bearer <token>",
                "Request headers, as name=value. One header on each line.",
            ),
            field("telemetry.metrics", "Export metrics", Kind::Check, ""),
            field("telemetry.logs", "Export events", Kind::Check, ""),
            field(
                "telemetry.metrics_interval",
                "Metrics interval",
                Kind::Duration,
                "",
            ),
            field(
                "telemetry.logs_interval",
                "Events interval",
                Kind::Duration,
                "",
            ),
            field(
                "telemetry.temporality",
                "Temporality",
                Kind::Choice(&["cumulative", "delta"]),
                "",
            ),
            field(
                "telemetry.prefix",
                "Prefix",
                Kind::Text,
                "The prefix of the metric and event names. claude_code gives the names of Claude Code.",
            ),
            field("telemetry.service_name", "Service name", Kind::Text, ""),
            hint(
                "telemetry.resource_attributes",
                "Resource attributes",
                Kind::Map,
                "team=platform",
                "Attributes of each metric and event, as name=value. One attribute on each line.",
            ),
            field(
                "telemetry.user_email",
                "User email",
                Kind::Text,
                "Empty for the email of the Anthropic sign-in.",
            ),
            field(
                "telemetry.account_uuid",
                "Account UUID",
                Kind::Text,
                "Empty for the account of the Anthropic sign-in.",
            ),
            field(
                "telemetry.organization_id",
                "Organization ID",
                Kind::Text,
                "Empty for the organization of the Anthropic sign-in.",
            ),
            field(
                "telemetry.include_account_uuid",
                "Include the account UUID and the account ID",
                Kind::Check,
                "",
            ),
            field(
                "telemetry.log_user_prompts",
                "Include the prompt text",
                Kind::Check,
                "",
            ),
            field(
                "telemetry.log_assistant_responses",
                "Include the response text",
                Kind::Check,
                "",
            ),
            field(
                "telemetry.log_tool_details",
                "Include the tool arguments, commands, and errors",
                Kind::Check,
                "",
            ),
            field(
                "telemetry.content_max_length",
                "Maximum text length",
                Kind::Integer,
                "The maximum number of characters of a text attribute.",
            ),
        ],
    },
];

pub fn preset_defaults() -> ConfigDocument {
    let mut defaults = match serde_json::to_value(AgentConfig::default()) {
        Ok(Value::Object(map)) => ConfigDocument::from(map),
        _ => ConfigDocument::default(),
    };
    defaults.set("policy.default", Value::from("allow"));
    defaults
}

pub fn has_comments(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with('#') || line.contains(" #")
    })
}

pub fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().map(scalar_text).collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![scalar_text(other)],
    }
}

pub fn pairs(value: Option<&Value>) -> Vec<(String, String)> {
    match value {
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| (key.clone(), scalar_text(value)))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn pairs_value(pairs: &[(String, String)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(key, value)| (key.clone(), Value::from(value.clone())))
            .collect::<Map<String, Value>>(),
    )
}

pub fn strings_value(items: &[String]) -> Value {
    Value::Array(items.iter().cloned().map(Value::from).collect())
}

pub fn text_of(kind: Kind, value: Option<&Value>) -> String {
    match kind {
        Kind::List => strings(value).join("\n"),
        Kind::Map => pairs(value)
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.map(scalar_text).unwrap_or_default(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub value: Option<Value>,
    pub error: Option<String>,
}

impl Parsed {
    fn ok(value: Option<Value>) -> Self {
        Self { value, error: None }
    }

    fn invalid(text: &str, error: impl Into<String>) -> Self {
        Self {
            value: Some(Value::from(text)),
            error: Some(error.into()),
        }
    }
}

pub fn parse_text(kind: Kind, text: &str) -> Parsed {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Parsed::ok(None);
    }
    match kind {
        Kind::Multiline => Parsed::ok(Some(Value::from(text.trim_end()))),
        Kind::Integer => match trimmed.parse::<u64>() {
            Ok(number) => Parsed::ok(Some(Value::from(number))),
            Err(_) => Parsed::invalid(trimmed, "Give a whole number, such as 100."),
        },
        Kind::Number => match trimmed.parse::<f64>() {
            Ok(number) if number.is_finite() => Parsed::ok(Some(Value::from(number))),
            _ => Parsed::invalid(trimmed, "Give a number, such as 0.8."),
        },
        Kind::Size => match ByteSize::from_str(trimmed) {
            Ok(_) => Parsed::ok(Some(Value::from(trimmed))),
            Err(_) => Parsed::invalid(trimmed, "Give a size, such as 64KiB or 1MiB."),
        },
        Kind::Duration => match HumanDuration::from_str(trimmed) {
            Ok(_) => Parsed::ok(Some(Value::from(trimmed))),
            Err(_) => Parsed::invalid(trimmed, "Give a duration, such as 30s, 10m, or 1h."),
        },
        Kind::List => Parsed::ok(Some(strings_value(
            &trimmed
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        ))),
        Kind::Map => {
            let mut map = Map::new();
            for line in trimmed.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let Some((key, value)) = line.split_once('=') else {
                    return Parsed {
                        value: None,
                        error: Some(format!("Write {line:?} as name=value.")),
                    };
                };
                let key = key.trim();
                if key.is_empty() {
                    return Parsed {
                        value: None,
                        error: Some(format!("Give a name in {line:?}.")),
                    };
                }
                map.insert(key.to_owned(), Value::from(value.trim()));
            }
            Parsed::ok(Some(Value::Object(map)))
        }
        _ => Parsed::ok(Some(Value::from(trimmed))),
    }
}

pub fn current<'a>(
    doc: &'a ConfigDocument,
    defaults: &'a ConfigDocument,
    path: &str,
) -> Option<&'a Value> {
    doc.get(path).or_else(|| defaults.get(path))
}

pub fn apply(
    doc: &mut ConfigDocument,
    defaults: &ConfigDocument,
    path: &str,
    value: Option<Value>,
) -> bool {
    let value = value.filter(|value| {
        let default = defaults.get(path);
        let empty = match value {
            Value::Array(items) => items.is_empty(),
            Value::Object(map) => map.is_empty(),
            _ => false,
        };
        default != Some(value) && !(empty && default.is_none())
    });
    let before = doc.get(path).cloned();
    match &value {
        Some(value) => doc.set(path, value.clone()),
        None => {
            doc.remove(path);
        }
    }
    before != value
}

pub fn choice_of(field: &Field, doc: &ConfigDocument, defaults: &ConfigDocument) -> String {
    current(doc, defaults, field.path)
        .map(scalar_text)
        .unwrap_or_default()
}

pub fn placeholder(field: &Field, defaults: &ConfigDocument) -> String {
    match defaults.get(field.path) {
        Some(value) if !matches!(value, Value::Array(_) | Value::Object(_)) => {
            let text = scalar_text(value);
            if text.is_empty() {
                field.placeholder.to_owned()
            } else {
                text
            }
        }
        _ => field.placeholder.to_owned(),
    }
}

pub fn rules(value: Option<&Value>) -> Result<Vec<PolicyRule>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value.clone()).map_err(|err| err.to_string()),
    }
}

pub fn rules_value(rules: &[PolicyRule]) -> Value {
    serde_json::to_value(rules).unwrap_or(Value::Array(Vec::new()))
}

pub fn action_name(action: PolicyAction) -> String {
    serde_json::to_value(action)
        .ok()
        .as_ref()
        .map(scalar_text)
        .unwrap_or_default()
}

pub fn parse_action(name: &str) -> Option<PolicyAction> {
    serde_json::from_value(Value::from(name)).ok()
}

pub fn rule_label(rule: &PolicyRule) -> String {
    let action = action_name(rule.action);
    if rule.command_prefix.is_empty() {
        format!("{}: {action}", rule.tool)
    } else {
        format!(
            "{} ({}): {action}",
            rule.tool,
            rule.command_prefix.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    fn resolve<'a>(schema: &'a Value, node: &'a Value) -> &'a Value {
        let reference = node.get("$ref").or_else(|| {
            node.get("allOf")
                .and_then(Value::as_array)
                .filter(|all| all.len() == 1)
                .and_then(|all| all[0].get("$ref"))
        });
        match reference.and_then(Value::as_str) {
            Some(reference) => {
                let name = reference.rsplit('/').next().unwrap_or_default();
                resolve(schema, &schema["$defs"][name])
            }
            None => node,
        }
    }

    fn leaves(schema: &Value, node: &Value, prefix: &str, out: &mut Vec<(String, Value)>) {
        let node = resolve(schema, node);
        let Some(properties) = node.get("properties").and_then(Value::as_object) else {
            out.push((prefix.to_owned(), node.clone()));
            return;
        };
        for (key, child) in properties {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            leaves(schema, child, &path, out);
        }
    }

    fn constants(schema: &Value, node: &Value, out: &mut BTreeSet<String>) {
        let node = resolve(schema, node);
        if let Some(Value::String(value)) = node.get("const") {
            out.insert(value.clone());
        }
        if let Some(values) = node.get("enum").and_then(Value::as_array) {
            out.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        for key in ["anyOf", "oneOf"] {
            for item in node
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                constants(schema, item, out);
            }
        }
    }

    fn covered(path: &str, list: &[&str]) -> bool {
        list.iter()
            .any(|key| path == *key || path.starts_with(&format!("{key}.")))
    }

    #[test]
    fn the_form_covers_each_setting_of_the_schema() {
        let schema = loom_config::agent_config_schema();
        let mut found = Vec::new();
        leaves(&schema, &schema, "", &mut found);
        let fields: Vec<&Field> = SECTIONS.iter().flat_map(|s| s.fields).collect();
        for (path, node) in &found {
            if covered(path, loom_config::MANAGED_SECTIONS) || covered(path, YAML_ONLY) {
                continue;
            }
            let field = fields
                .iter()
                .find(|f| f.path == path)
                .unwrap_or_else(|| panic!("the form has no field for {path}"));
            if let Kind::Choice(options) | Kind::OptionalChoice(options) = field.kind {
                let mut expected = BTreeSet::new();
                constants(&schema, node, &mut expected);
                let options: BTreeSet<String> = options.iter().map(|o| o.to_string()).collect();
                assert_eq!(options, expected, "the choices of {path}");
            }
        }
        for field in fields {
            assert!(
                found.iter().any(|(path, _)| path == field.path),
                "{} is not in the schema",
                field.path
            );
        }
    }

    #[test]
    fn each_choice_default_is_a_choice() {
        let defaults = preset_defaults();
        for field in SECTIONS.iter().flat_map(|s| s.fields) {
            if let Kind::Choice(options) = field.kind {
                let current = choice_of(field, &ConfigDocument::default(), &defaults);
                assert!(options.contains(&current.as_str()), "{}", field.path);
            }
        }
        assert_eq!(defaults.get("policy.default"), Some(&json!("allow")));
    }

    #[test]
    fn texts_become_values() {
        assert_eq!(parse_text(Kind::Integer, " 12 ").value, Some(json!(12)));
        let bad = parse_text(Kind::Integer, "many");
        assert_eq!(bad.value, Some(json!("many")));
        assert!(bad.error.is_some());
        assert_eq!(parse_text(Kind::Number, "0.5").value, Some(json!(0.5)));
        assert_eq!(parse_text(Kind::Size, "1MiB").value, Some(json!("1MiB")));
        assert!(parse_text(Kind::Size, "lots").error.is_some());
        assert_eq!(parse_text(Kind::Duration, "10m").value, Some(json!("10m")));
        assert!(parse_text(Kind::Duration, "soon").error.is_some());
        assert_eq!(parse_text(Kind::Text, "  ").value, None);
        assert_eq!(
            parse_text(Kind::List, "a\n\n b \n").value,
            Some(json!(["a", "b"]))
        );
        assert_eq!(
            parse_text(Kind::Map, "A=1\nB = x=y\n").value,
            Some(json!({"A": "1", "B": "x=y"}))
        );
        assert!(parse_text(Kind::Map, "A").error.is_some());
        assert_eq!(text_of(Kind::Map, Some(&json!({"A": "1"}))), "A=1");
        assert_eq!(text_of(Kind::List, Some(&json!(["a", "b"]))), "a\nb");
        assert_eq!(text_of(Kind::Integer, Some(&json!(5))), "5");
    }

    #[test]
    fn defaults_are_left_out() {
        let defaults = preset_defaults();
        let mut doc =
            ConfigDocument::parse("tools:\n  read:\n    max_lines: 10\n", "agent.yaml").unwrap();
        assert!(apply(
            &mut doc,
            &defaults,
            "tools.read.max_lines",
            Some(json!(2000))
        ));
        assert!(doc.is_empty());
        assert!(apply(
            &mut doc,
            &defaults,
            "policy.default",
            Some(json!("ask"))
        ));
        assert!(!apply(
            &mut doc,
            &defaults,
            "policy.default",
            Some(json!("ask"))
        ));
        assert!(apply(
            &mut doc,
            &defaults,
            "policy.default",
            Some(json!("allow"))
        ));
        assert!(!apply(&mut doc, &defaults, "skills.paths", Some(json!([]))));
        assert!(doc.is_empty());
        assert_eq!(
            placeholder(&SECTIONS[3].fields[1], &defaults),
            "2000",
            "{}",
            SECTIONS[3].fields[1].path
        );
    }

    #[test]
    fn rules_have_labels() {
        let defaults = preset_defaults();
        let rules = rules(defaults.get("policy.rules")).unwrap();
        assert_eq!(rule_label(&rules[0]), "read: allow");
        let rule = PolicyRule {
            tool: "shell".into(),
            action: parse_action("allow_harmful").unwrap(),
            command_prefix: vec!["git push".into()],
        };
        assert_eq!(rule_label(&rule), "shell (git push): allow_harmful");
        assert_eq!(rules_value(&[rule])[0]["action"], json!("allow_harmful"));
        assert!(has_comments("a: 1 # one\n"));
        assert!(!has_comments("a: \"x#y\"\n"));
    }
}
