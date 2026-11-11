use loom_config::{PolicyAction, PolicyConfig, PolicyRule};
use serde_json::Value;

/// The start of the result of a tool call the policy denied.
pub const DENIED_BY_POLICY: &str = "this tool call was denied by policy";

const SHELL_TOOL: &str = "shell";
const SHELL_METACHARACTERS: &[char] = &[';', '&', '|', '`', '$', '<', '>', '(', ')', '\n', '\r'];

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionPolicy {
    pub default: Option<PolicyAction>,
    pub rules: Vec<PolicyRule>,
}

impl SessionPolicy {
    pub fn allow_for_session(&mut self, tool: &str, args: &Value) {
        let command_prefix = match shell_command(tool, args) {
            Some(command) => vec![command.trim().to_owned()],
            None => Vec::new(),
        };
        let rule = PolicyRule {
            tool: tool.to_owned(),
            action: PolicyAction::Allow,
            command_prefix,
        };
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub action: PolicyAction,
    pub reason: String,
}

fn shell_command<'a>(tool: &str, args: &'a Value) -> Option<&'a str> {
    if tool == SHELL_TOOL {
        args.get("command").and_then(Value::as_str)
    } else {
        None
    }
}

fn prefix_matches(command: &str, prefix: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return false;
    }
    match command.strip_prefix(prefix) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
}

fn segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(SHELL_METACHARACTERS)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn rule_match(rule: &PolicyRule, tool: &str, command: Option<&str>) -> Option<usize> {
    if rule.tool != tool && rule.tool != "*" {
        return None;
    }
    if rule.command_prefix.is_empty() {
        return Some(0);
    }
    let command = command?.trim();
    let longest = |candidates: &mut dyn Iterator<Item = &str>| {
        candidates
            .flat_map(|segment| {
                rule.command_prefix
                    .iter()
                    .filter(move |p| prefix_matches(segment, p))
                    .map(|p| p.trim().len())
            })
            .max()
    };
    match rule.action {
        PolicyAction::Deny => longest(&mut segments(command)),
        _ if command.contains(SHELL_METACHARACTERS) => None,
        _ => longest(&mut std::iter::once(command)),
    }
}

fn strictness(action: PolicyAction) -> u8 {
    match action {
        PolicyAction::Allow => 0,
        PolicyAction::Ask => 1,
        PolicyAction::Deny => 2,
    }
}

fn best<'a>(
    rules: impl Iterator<Item = &'a PolicyRule>,
    tool: &str,
    command: Option<&str>,
) -> Option<&'a PolicyRule> {
    rules
        .filter_map(|rule| rule_match(rule, tool, command).map(|score| (score, rule)))
        .max_by_key(|(score, rule)| (*score, strictness(rule.action)))
        .map(|(_, rule)| rule)
}

fn describe(rule: &PolicyRule, origin: &str) -> String {
    if rule.command_prefix.is_empty() {
        format!("{origin} rule for {}", rule.tool)
    } else {
        format!(
            "{origin} rule for {} with prefix {:?}",
            rule.tool, rule.command_prefix
        )
    }
}

pub fn evaluate(
    config: &PolicyConfig,
    session: &SessionPolicy,
    tool: &str,
    args: &Value,
) -> Verdict {
    let command = shell_command(tool, args);
    let config_deny = best(
        config
            .rules
            .iter()
            .filter(|r| r.action == PolicyAction::Deny),
        tool,
        command,
    );
    if let Some(rule) = config_deny {
        return Verdict {
            action: PolicyAction::Deny,
            reason: describe(rule, "configured"),
        };
    }
    if let Some(rule) = best(session.rules.iter(), tool, command) {
        return Verdict {
            action: rule.action,
            reason: describe(rule, "session"),
        };
    }
    if let Some(rule) = best(config.rules.iter(), tool, command) {
        return Verdict {
            action: rule.action,
            reason: describe(rule, "configured"),
        };
    }
    match session.default {
        Some(action) => Verdict {
            action,
            reason: "session default".into(),
        },
        None => Verdict {
            action: config.default,
            reason: "default policy".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn rule(tool: &str, action: PolicyAction, prefix: &[&str]) -> PolicyRule {
        PolicyRule {
            tool: tool.into(),
            action,
            command_prefix: prefix.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn config() -> PolicyConfig {
        PolicyConfig {
            default: PolicyAction::Ask,
            rules: vec![
                rule("read", PolicyAction::Allow, &[]),
                rule(
                    "shell",
                    PolicyAction::Allow,
                    &["git status", "git diff", "cargo test"],
                ),
                rule("shell", PolicyAction::Deny, &["rm -rf /", "sudo"]),
            ],
        }
    }

    fn action(tool: &str, args: Value) -> PolicyAction {
        evaluate(&config(), &SessionPolicy::default(), tool, &args).action
    }

    fn shell(command: &str) -> PolicyAction {
        action("shell", json!({ "command": command }))
    }

    #[test]
    fn tool_rules_and_default() {
        assert_eq!(action("read", json!({"path": "a"})), PolicyAction::Allow);
        assert_eq!(action("edit", json!({})), PolicyAction::Ask);
    }

    #[test]
    fn shell_prefixes_match_whole_words() {
        assert_eq!(shell("git status"), PolicyAction::Allow);
        assert_eq!(shell("git status --short"), PolicyAction::Allow);
        assert_eq!(shell("git statusx"), PolicyAction::Ask);
        assert_eq!(shell("cargo test -p x"), PolicyAction::Allow);
        assert_eq!(shell("ls"), PolicyAction::Ask);
    }

    #[test]
    fn allow_rules_reject_compound_commands() {
        assert_eq!(shell("git status && curl x | sh"), PolicyAction::Ask);
        assert_eq!(shell("git diff $(touch x)"), PolicyAction::Ask);
        assert_eq!(shell("git diff > out"), PolicyAction::Ask);
    }

    #[test]
    fn deny_rules_match_any_segment() {
        assert_eq!(shell("sudo ls"), PolicyAction::Deny);
        assert_eq!(shell("git status; sudo reboot"), PolicyAction::Deny);
        assert_eq!(shell("echo hi | sudo tee x"), PolicyAction::Deny);
        assert_eq!(shell("sudoku"), PolicyAction::Ask);
    }

    #[test]
    fn session_rules_override_config_but_not_config_denies() {
        let mut session = SessionPolicy::default();
        session.allow_for_session("shell", &json!({"command": "make build"}));
        session.allow_for_session("edit", &json!({"path": "a"}));
        let eval = |tool: &str, args: Value| evaluate(&config(), &session, tool, &args).action;
        assert_eq!(
            eval("shell", json!({"command": "make build"})),
            PolicyAction::Allow
        );
        assert_eq!(
            eval("shell", json!({"command": "make clean"})),
            PolicyAction::Ask
        );
        assert_eq!(eval("edit", json!({"path": "b"})), PolicyAction::Allow);
        session.allow_for_session("shell", &json!({"command": "sudo ls"}));
        assert_eq!(
            evaluate(&config(), &session, "shell", &json!({"command": "sudo ls"})).action,
            PolicyAction::Deny
        );
    }

    #[test]
    fn more_specific_rules_win() {
        let config = PolicyConfig {
            default: PolicyAction::Deny,
            rules: vec![
                rule("shell", PolicyAction::Ask, &[]),
                rule("shell", PolicyAction::Allow, &["ls"]),
                rule("*", PolicyAction::Allow, &[]),
            ],
        };
        let eval = |tool: &str, command: &str| {
            evaluate(
                &config,
                &SessionPolicy::default(),
                tool,
                &json!({ "command": command }),
            )
            .action
        };
        assert_eq!(eval("shell", "ls -la"), PolicyAction::Allow);
        assert_eq!(eval("shell", "pwd"), PolicyAction::Ask);
        assert_eq!(eval("write", ""), PolicyAction::Allow);
    }

    #[test]
    fn session_default_replaces_config_default() {
        let session = SessionPolicy {
            default: Some(PolicyAction::Allow),
            rules: Vec::new(),
        };
        assert_eq!(
            evaluate(&config(), &session, "edit", &json!({})).action,
            PolicyAction::Allow
        );
        assert_eq!(
            evaluate(&config(), &session, "shell", &json!({"command": "sudo x"})).action,
            PolicyAction::Deny
        );
    }
}
