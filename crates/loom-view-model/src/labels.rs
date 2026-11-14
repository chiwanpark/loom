//! Names for session settings, as the clients show them.

use loom_proto::{PolicyAction, ReasoningEffort};

pub const REASONING_CHOICES: [(Option<ReasoningEffort>, &str); 7] = [
    (None, "Default"),
    (Some(ReasoningEffort::None), "None"),
    (Some(ReasoningEffort::Minimal), "Minimal"),
    (Some(ReasoningEffort::Low), "Low"),
    (Some(ReasoningEffort::Medium), "Medium"),
    (Some(ReasoningEffort::High), "High"),
    (Some(ReasoningEffort::Xhigh), "Extra high"),
];

pub const POLICY_CHOICES: [(Option<PolicyAction>, &str); 4] = [
    (None, "Default"),
    (Some(PolicyAction::Ask), "Ask"),
    (Some(PolicyAction::Allow), "Allow"),
    (Some(PolicyAction::Deny), "Deny"),
];

pub fn reasoning_label(effort: Option<ReasoningEffort>) -> &'static str {
    REASONING_CHOICES
        .iter()
        .find(|(value, _)| *value == effort)
        .map(|(_, label)| *label)
        .unwrap_or("Default")
}

pub fn policy_label(policy: Option<PolicyAction>) -> &'static str {
    POLICY_CHOICES
        .iter()
        .find(|(value, _)| *value == policy)
        .map(|(_, label)| *label)
        .unwrap_or("Default")
}
