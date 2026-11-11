use loom_llm::{CompletionRequest, InputContent, Message};

pub(crate) const SUMMARY_PREFIX: &str = "Summary of the earlier conversation:\n";
const SUMMARY_SYSTEM: &str = "You summarize a coding session so that it can continue with less context. Keep the user's goals and preferences, decisions, file paths, commands, errors, and open questions. Write concise plain text without a preamble.";
const SUMMARY_REQUEST: &str =
    "Summarize the conversation above so that you can continue the work from the summary alone.";
const KEEP_TOOL_RESULTS: usize = 4;
pub(crate) const ELIDED: &str = "[output removed to save context]";
const CHARS_PER_TOKEN: u64 = 4;

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

fn is_summary(message: &Message) -> bool {
    match message {
        Message::User { content } => content.iter().any(|part| match part {
            InputContent::Text { text } => text.starts_with(SUMMARY_PREFIX),
            _ => false,
        }),
        _ => false,
    }
}

fn elidable(history: &[Message]) -> Vec<usize> {
    let results: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| match m {
            Message::ToolResult { content, .. } => !content.iter().all(|part| match part {
                InputContent::Text { text } => text == ELIDED,
                _ => false,
            }),
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

pub(crate) fn apply_summary(history: &mut Vec<Message>, split: usize, summary: &str) {
    let rest = history.split_off(split);
    *history = vec![Message::User {
        content: vec![InputContent::text(format!("{SUMMARY_PREFIX}{summary}"))],
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
}
