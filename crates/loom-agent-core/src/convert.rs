use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use loom_llm::{AssistantContent, AssistantMessage, InputContent};
use loom_proto::{
    ContentPart, OutputChunk, OutputStream, Risk, ToolResult, ToolResultDetails, Usage,
};
use loom_tools::{ToolDetails, ToolOutput};

pub fn input_content(part: &ContentPart) -> InputContent {
    match part {
        ContentPart::Text { text } => InputContent::Text { text: text.clone() },
        ContentPart::Image { mime_type, data } => InputContent::Image {
            mime_type: mime_type.clone(),
            data: STANDARD.encode(data),
        },
    }
}

pub fn risk(risk: loom_tools::Risk) -> Risk {
    match risk {
        loom_tools::Risk::ReadOnly => Risk::ReadOnly,
        loom_tools::Risk::Write => Risk::Write,
        loom_tools::Risk::Exec => Risk::Exec,
    }
}

pub fn output_chunk(chunk: loom_tools::OutputChunk) -> OutputChunk {
    OutputChunk {
        stream: match chunk.stream {
            loom_tools::OutputStream::Stdout => OutputStream::Stdout,
            loom_tools::OutputStream::Stderr => OutputStream::Stderr,
        },
        data: chunk.data,
    }
}

pub fn usage(usage: &loom_llm::Usage) -> Usage {
    Usage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        total: usage.total,
    }
}

pub fn reasoning_summary(message: &AssistantMessage) -> Option<String> {
    let parts: Vec<&str> = message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Reasoning { summary, .. } if !summary.trim().is_empty() => {
                Some(summary.trim())
            }
            _ => None,
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

pub fn tool_result(output: ToolOutput) -> ToolResult {
    let details = match output.details {
        ToolDetails::Read {
            path,
            start_line,
            end_line,
            total_lines,
            truncated,
        } => ToolResultDetails::Read {
            path,
            start_line: start_line as u64,
            end_line: end_line as u64,
            total_lines: total_lines as u64,
            truncated,
        },
        ToolDetails::Edit {
            path,
            diff,
            first_changed_line,
        } => ToolResultDetails::Edit {
            path,
            diff,
            first_changed_line: first_changed_line.map(|l| l as u64),
        },
        ToolDetails::Write {
            path,
            bytes,
            created,
        } => ToolResultDetails::Write {
            path,
            bytes: bytes as u64,
            created,
        },
        ToolDetails::Shell {
            exit_code,
            signal,
            timed_out,
            cancelled,
            truncated,
            total_lines,
            total_bytes,
            full_output_path,
            duration_ms,
        } => ToolResultDetails::Shell {
            exit_code,
            signal,
            timed_out,
            cancelled,
            truncated,
            total_lines: total_lines as u64,
            total_bytes,
            full_output_path,
            duration_ms,
        },
    };
    ToolResult {
        content: output.content,
        is_error: output.is_error,
        details: Some(details),
    }
}

pub fn error_result(message: impl Into<String>) -> ToolResult {
    ToolResult {
        content: message.into(),
        is_error: true,
        details: None,
    }
}

pub fn cap_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}\n\n[output truncated to {max_chars} characters]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_images_as_base64() {
        let part = ContentPart::Image {
            mime_type: "image/png".into(),
            data: vec![0xff, 0x00],
        };
        assert_eq!(
            input_content(&part),
            InputContent::Image {
                mime_type: "image/png".into(),
                data: "/wA=".into()
            }
        );
    }

    #[test]
    fn caps_on_char_boundaries() {
        assert_eq!(cap_chars("short", 10), "short");
        let capped = cap_chars("가나다라", 2);
        assert!(capped.starts_with("가나\n"));
        assert!(capped.contains("truncated"));
    }
}
