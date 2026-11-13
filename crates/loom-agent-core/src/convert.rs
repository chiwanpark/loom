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
        // The sources are in the content; clients have no view of their own.
        ToolDetails::WebSearch { .. } => None,
        // Not files: clients must not count them among the workspace changes.
        ToolDetails::Notes { .. } | ToolDetails::Tasks { .. } => None,
        ToolDetails::Read {
            path,
            start_line,
            end_line,
            total_lines,
            truncated,
        } => Some(ToolResultDetails::Read {
            path,
            start_line: start_line as u64,
            end_line: end_line as u64,
            total_lines: total_lines as u64,
            truncated,
        }),
        ToolDetails::Edit {
            path,
            diff,
            first_changed_line,
        } => Some(ToolResultDetails::Edit {
            path,
            diff,
            first_changed_line: first_changed_line.map(|l| l as u64),
        }),
        ToolDetails::Write {
            path,
            bytes,
            created,
            diff: Some(diff),
        } => Some(ToolResultDetails::WriteWithDiff {
            path,
            bytes: bytes as u64,
            created,
            diff,
        }),
        ToolDetails::Write {
            path,
            bytes,
            created,
            diff: None,
        } => Some(ToolResultDetails::Write {
            path,
            bytes: bytes as u64,
            created,
        }),
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
        } => Some(ToolResultDetails::Shell {
            exit_code,
            signal,
            timed_out,
            cancelled,
            truncated,
            total_lines: total_lines as u64,
            total_bytes,
            full_output_path,
            duration_ms,
        }),
    };
    ToolResult {
        content: output.content,
        is_error: output.is_error,
        details,
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

pub fn reasoning_to_proto(effort: loom_llm::ReasoningEffort) -> loom_proto::ReasoningEffort {
    use loom_llm::ReasoningEffort as L;
    use loom_proto::ReasoningEffort as P;
    match effort {
        L::None => P::None,
        L::Minimal => P::Minimal,
        L::Low => P::Low,
        L::Medium => P::Medium,
        L::High => P::High,
        L::Xhigh => P::Xhigh,
    }
}

pub fn reasoning_from_proto(effort: loom_proto::ReasoningEffort) -> loom_llm::ReasoningEffort {
    use loom_llm::ReasoningEffort as L;
    use loom_proto::ReasoningEffort as P;
    match effort {
        P::None => L::None,
        P::Minimal => L::Minimal,
        P::Low => L::Low,
        P::Medium => L::Medium,
        P::High => L::High,
        P::Xhigh => L::Xhigh,
    }
}

pub fn policy_to_proto(action: loom_config::PolicyAction) -> loom_proto::PolicyAction {
    match action {
        loom_config::PolicyAction::Allow => loom_proto::PolicyAction::Allow,
        loom_config::PolicyAction::Ask => loom_proto::PolicyAction::Ask,
        loom_config::PolicyAction::Deny => loom_proto::PolicyAction::Deny,
    }
}

pub fn policy_from_proto(action: loom_proto::PolicyAction) -> loom_config::PolicyAction {
    match action {
        loom_proto::PolicyAction::Allow => loom_config::PolicyAction::Allow,
        loom_proto::PolicyAction::Ask => loom_config::PolicyAction::Ask,
        loom_proto::PolicyAction::Deny => loom_config::PolicyAction::Deny,
    }
}

pub fn api_error(err: &crate::CoreError) -> loom_proto::ApiError {
    use crate::CoreError;
    use loom_proto::ErrorCode;
    let code = match err {
        CoreError::Busy => ErrorCode::Busy,
        CoreError::NoModel
        | CoreError::EmptyPrompt
        | CoreError::NothingToCompact
        | CoreError::NotesTooLarge { .. }
        | CoreError::TooManyTasks { .. }
        | CoreError::EmptyTask => ErrorCode::BadRequest,
        CoreError::NotesChanged => ErrorCode::Conflict,
        CoreError::NoSuchTask { .. } => ErrorCode::NotFound,
        CoreError::ConfigRejected(_) => ErrorCode::Config,
        CoreError::NoConfigFile => ErrorCode::Unsupported,
        CoreError::UnknownSession(_) | CoreError::UnknownApproval(_) => ErrorCode::NotFound,
        CoreError::Llm(err) => return llm_error(err),
        CoreError::Tool(_) => ErrorCode::Internal,
    };
    loom_proto::ApiError::new(code, err.to_string())
}

pub fn llm_error(err: &loom_llm::LlmError) -> loom_proto::ApiError {
    use loom_llm::LlmError;
    use loom_proto::ErrorCode;
    let code = match err {
        LlmError::UnknownProvider(_) | LlmError::InvalidModel(_) => ErrorCode::BadRequest,
        LlmError::Config { .. } => ErrorCode::Config,
        LlmError::Unsupported(_) => ErrorCode::Unsupported,
        LlmError::NotAuthenticated { .. } | LlmError::Login(_) => ErrorCode::NotAuthenticated,
        LlmError::Cancelled => ErrorCode::Cancelled,
        LlmError::Http(_)
        | LlmError::Api { .. }
        | LlmError::Stream { .. }
        | LlmError::Protocol { .. } => ErrorCode::Provider,
        LlmError::Store { .. } | LlmError::Io { .. } => ErrorCode::Internal,
    };
    loom_proto::ApiError::new(code, err.to_string())
}

pub fn login_method(method: loom_proto::LoginMethod) -> loom_llm::LoginMethod {
    match method {
        loom_proto::LoginMethod::Browser {
            redirect_uri,
            account,
        } => loom_llm::LoginMethod::Browser {
            redirect_uri,
            account,
        },
        loom_proto::LoginMethod::DeviceCode => loom_llm::LoginMethod::DeviceCode,
    }
}

pub fn login_input(input: loom_proto::LoginInput) -> loom_llm::LoginInput {
    match input {
        loom_proto::LoginInput::Redirect { url } => loom_llm::LoginInput::Redirect { url },
        loom_proto::LoginInput::Code { code } => loom_llm::LoginInput::Code { code },
    }
}

pub fn login_started(started: loom_llm::LoginStarted) -> loom_proto::LoginStarted {
    loom_proto::LoginStarted {
        login_id: started.login_id,
        prompt: match started.prompt {
            loom_llm::LoginPrompt::OpenUrl { url, redirect_uri } => {
                loom_proto::LoginPrompt::OpenUrl { url, redirect_uri }
            }
            loom_llm::LoginPrompt::DeviceCode {
                user_code,
                verification_uri,
                expires_in_secs,
            } => loom_proto::LoginPrompt::DeviceCode {
                user_code,
                verification_uri,
                expires_in_secs,
            },
        },
    }
}

pub fn account_info(info: loom_llm::AccountInfo) -> loom_proto::AccountInfo {
    loom_proto::AccountInfo {
        provider: info.provider.to_string(),
        account_id: info.account_id,
        label: info.label,
    }
}

pub fn usage_limits(limits: loom_llm::UsageLimits) -> loom_proto::UsageLimits {
    loom_proto::UsageLimits {
        limits: limits
            .limits
            .into_iter()
            .map(|l| loom_proto::UsageLimit {
                label: l.label,
                used_percent: l.used_percent,
                detail: l.detail,
                resets_at: l.resets_at,
                headline: l.headline,
            })
            .collect(),
    }
}

pub fn provider_status(
    status: loom_llm::ProviderStatus,
    enabled: bool,
) -> loom_proto::ProviderStatus {
    loom_proto::ProviderStatus {
        provider: status.provider.to_string(),
        display_name: status.display_name,
        enabled,
        login_methods: status
            .login_methods
            .into_iter()
            .map(|m| match m {
                loom_llm::LoginMethodInfo::Browser { redirect } => {
                    loom_proto::LoginMethodInfo::Browser {
                        redirect: loom_proto::RedirectSpec {
                            host: redirect.host,
                            port: redirect.port,
                            path: redirect.path,
                        },
                    }
                }
                loom_llm::LoginMethodInfo::DeviceCode => loom_proto::LoginMethodInfo::DeviceCode,
                loom_llm::LoginMethodInfo::ApiKey => loom_proto::LoginMethodInfo::ApiKey,
            })
            .collect(),
        api_key_set: status.api_key_set,
        accounts: status
            .accounts
            .into_iter()
            .map(|a| loom_proto::AccountStatus {
                id: a.id,
                label: a.label,
                active: a.active,
                signed_in: a.signed_in,
            })
            .collect(),
    }
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

    #[test]
    fn write_details_carry_a_diff_when_available() {
        let output = |diff: Option<&str>| ToolOutput {
            content: "Wrote a.txt".into(),
            is_error: false,
            details: ToolDetails::Write {
                path: "a.txt".into(),
                bytes: 3,
                created: false,
                diff: diff.map(str::to_owned),
            },
        };
        assert!(matches!(
            tool_result(output(Some("-a\n+b\n"))).details,
            Some(ToolResultDetails::WriteWithDiff { ref diff, .. }) if diff == "-a\n+b\n"
        ));
        assert!(matches!(
            tool_result(output(None)).details,
            Some(ToolResultDetails::Write { bytes: 3, .. })
        ));
    }
}
