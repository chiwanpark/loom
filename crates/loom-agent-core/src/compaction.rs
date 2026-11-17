use loom_llm::{CompletionRequest, InputContent, Message};

use crate::skills::{CONTENT_START, loaded_instructions};

pub(crate) const SUMMARY_PREFIX: &str = "Summary of the earlier conversation:\n";
const SUMMARY_SYSTEM: &str = "You summarize a coding session so that it can continue with less context. Keep the user's goals and preferences, decisions, file paths, commands, errors, and open questions. Write concise plain text without a preamble.";
const SUMMARY_REQUEST: &str =
    "Summarize the conversation above so that you can continue the work from the summary alone.";
const KEEP_TOOL_RESULTS: usize = 4;
pub(crate) const ELIDED: &str = "[output removed to save context]";
const CHARS_PER_TOKEN: u64 = 4;
const SKILLS_KEPT: &str =
    "The instructions of the skills that were loaded earlier, which still apply:";

pub(crate) fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
        .sum();
    chars as u64 / CHARS_PER_TOKEN
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Summarize { split: usize },
    Elide,
    Nothing,
}

pub(crate) fn plan(history: &[Message]) -> Plan {
    let split = history
        .iter()
        .rposition(|m| matches!(m, Message::User { .. }))
        .unwrap_or(0);
    let summarized_only = split == 1 && is_summary(&history[0]);
    if split > 0 && !summarized_only {
        return Plan::Summarize { split };
    }
    if !elidable(history).is_empty() {
        Plan::Elide
    } else {
        Plan::Nothing
    }
}

pub(crate) fn is_summary(message: &Message) -> bool {
    match message {
        Message::User { content } => content.iter().any(|part| match part {
            InputContent::Text { text } => text.starts_with(SUMMARY_PREFIX),
            _ => false,
        }),
        _ => false,
    }
}

fn texts(message: &Message) -> impl Iterator<Item = &str> {
    let content: &[InputContent] = match message {
        Message::User { content } | Message::ToolResult { content, .. } => content,
        _ => &[],
    };
    content.iter().filter_map(|part| match part {
        InputContent::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

/// Whether a tool result holds a skill's instructions, which stay in the
/// context as long as the turns around them.
fn is_skill(message: &Message) -> bool {
    matches!(message, Message::ToolResult { .. })
        && texts(message).any(|text| text.starts_with(CONTENT_START))
}

fn elidable(history: &[Message]) -> Vec<usize> {
    let results: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| match m {
            Message::ToolResult { content, .. } => {
                !is_skill(m)
                    && !content.iter().all(|part| match part {
                        InputContent::Text { text } => text == ELIDED,
                        _ => false,
                    })
            }
            _ => false,
        })
        .map(|(i, _)| i)
        .collect();
    let keep = results.len().saturating_sub(KEEP_TOOL_RESULTS);
    results[..keep].to_vec()
}

pub(crate) fn elide_tool_results(history: &mut [Message]) -> usize {
    let targets = elidable(history);
    for index in &targets {
        if let Message::ToolResult { content, .. } = &mut history[*index] {
            *content = vec![InputContent::text(ELIDED)];
        }
    }
    targets.len()
}

pub(crate) fn summary_request(model: &str, prefix: &[Message]) -> CompletionRequest {
    let mut messages = prefix.to_vec();
    messages.push(Message::User {
        content: vec![InputContent::text(SUMMARY_REQUEST)],
    });
    CompletionRequest {
        model: model.to_owned(),
        system: Some(SUMMARY_SYSTEM.into()),
        messages,
        tools: Vec::new(),
        reasoning: None,
        max_output_tokens: None,
        session_id: None,
        capabilities: Default::default(),
    }
}

/// Replaces the turns before `split` with their summary. The instructions
/// of the skills loaded in them go after the summary, so the model keeps
/// following them.
pub(crate) fn apply_summary(history: &mut Vec<Message>, split: usize, summary: &str) {
    let skills = loaded_instructions(history[..split].iter().flat_map(texts));
    let rest = history.split_off(split);
    let mut text = format!("{SUMMARY_PREFIX}{summary}");
    if !skills.is_empty() {
        text.push_str(&format!("\n\n{SKILLS_KEPT}\n\n{}", skills.join("\n\n")));
    }
    *history = vec![Message::User {
        content: vec![InputContent::text(text)],
    }];
    history.extend(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![InputContent::text(text)],
        }
    }

    fn result(id: &str) -> Message {
        Message::ToolResult {
            call_id: id.into(),
            tool_name: "read".into(),
            content: vec![InputContent::text("x".repeat(100))],
            is_error: false,
        }
    }

    #[test]
    fn plans_summaries_before_the_current_turn() {
        assert_eq!(plan(&[]), Plan::Nothing);
        assert_eq!(plan(&[user("a")]), Plan::Nothing);
        assert_eq!(plan(&[user("a"), user("b")]), Plan::Summarize { split: 1 });
        let mut history = vec![user("a"), user("b"), user("c")];
        apply_summary(&mut history, 2, "short");
        assert_eq!(history.len(), 2);
        assert!(is_summary(&history[0]));
        assert_eq!(plan(&history), Plan::Nothing);
    }

    #[test]
    fn elides_old_tool_results_in_a_long_turn() {
        let mut history = vec![user("go")];
        for i in 0..6 {
            history.push(result(&format!("c{i}")));
        }
        assert_eq!(plan(&history), Plan::Elide);
        let before = estimate_tokens(&history);
        assert_eq!(elide_tool_results(&mut history), 2);
        assert!(estimate_tokens(&history) < before);
        assert_eq!(elide_tool_results(&mut history), 0);
        assert_eq!(plan(&history), Plan::Nothing);
    }

    fn skill(id: &str, name: &str, body: &str) -> Message {
        Message::ToolResult {
            call_id: id.into(),
            tool_name: "skill".into(),
            content: vec![InputContent::text(format!(
                "<skill_content name=\"{name}\">\n{body}\n</skill_content>"
            ))],
            is_error: false,
        }
    }

    #[test]
    fn skill_instructions_are_not_elided() {
        let mut history = vec![user("go"), skill("s", "pdf", "PDF RULES")];
        for i in 0..6 {
            history.push(result(&format!("c{i}")));
        }
        assert_eq!(elide_tool_results(&mut history), 2);
        assert_eq!(history[1], skill("s", "pdf", "PDF RULES"));
    }

    #[test]
    fn a_summary_keeps_the_loaded_skills() {
        let mut history = vec![
            user("a"),
            skill("s1", "pdf", "OLD PDF RULES"),
            skill("s2", "review", "REVIEW RULES"),
            skill("s3", "pdf", "PDF RULES"),
            user("b"),
        ];
        apply_summary(&mut history, 4, "short");
        assert_eq!(history.len(), 2);
        let text: Vec<&str> = texts(&history[0]).collect();
        let text = text.join("");
        assert!(
            text.starts_with(&format!("{SUMMARY_PREFIX}short\n\n{SKILLS_KEPT}")),
            "{text}"
        );
        assert!(!text.contains("OLD PDF RULES"), "{text}");
        assert!(text.find("REVIEW RULES").unwrap() < text.find("PDF RULES").unwrap());
        // A later summary carries them over again.
        history.push(user("c"));
        apply_summary(&mut history, 2, "shorter");
        let text: String = texts(&history[0]).collect();
        assert!(
            text.contains("REVIEW RULES") && text.contains("PDF RULES"),
            "{text}"
        );
    }
}
