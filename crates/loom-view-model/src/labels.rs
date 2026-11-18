//! Names for session settings, as the clients show them.

use loom_proto::{PolicyAction, ReasoningEffort};

pub const REASONING_CHOICES: [(ReasoningEffort, &str); 6] = [
    (ReasoningEffort::None, "None"),
    (ReasoningEffort::Minimal, "Minimal"),
    (ReasoningEffort::Low, "Low"),
    (ReasoningEffort::Medium, "Medium"),
    (ReasoningEffort::High, "High"),
    (ReasoningEffort::Xhigh, "Extra high"),
];

pub const POLICY_CHOICES: [(PolicyAction, &str); 4] = [
    (PolicyAction::Ask, "Ask"),
    (PolicyAction::Allow, "Allow"),
    (PolicyAction::AllowHarmful, "Allow harmful"),
    (PolicyAction::Deny, "Deny"),
];

pub const COMMENT_GUARD_CHOICES: [(bool, &str); 2] = [(true, "On"), (false, "Off")];

pub fn reasoning_label(effort: Option<ReasoningEffort>) -> &'static str {
    REASONING_CHOICES
        .iter()
        .find(|(value, _)| Some(*value) == effort)
        .map(|(_, label)| *label)
        .unwrap_or("Model default")
}

pub fn policy_label(policy: PolicyAction) -> &'static str {
    match policy {
        PolicyAction::Ask => "Ask",
        PolicyAction::Allow => "Allow",
        PolicyAction::AllowHarmful => "Allow harmful",
        PolicyAction::Deny => "Deny",
    }
}

pub fn comment_guard_label(enabled: bool) -> &'static str {
    if enabled { "On" } else { "Off" }
}
