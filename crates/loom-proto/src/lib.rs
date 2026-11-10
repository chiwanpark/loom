pub mod session;

pub use session::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: SessionEvent) {
        let bytes = postcard::to_allocvec(&SequencedEvent {
            seq: 7,
            event: event.clone(),
        })
        .unwrap();
        let decoded: SequencedEvent = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.seq, 7);
        assert_eq!(decoded.event, event);
    }

    #[test]
    fn events_roundtrip_through_postcard() {
        roundtrip(SessionEvent::UserMessage {
            content: vec![
                ContentPart::text("hi"),
                ContentPart::Image {
                    mime_type: "image/png".into(),
                    data: vec![1, 2, 3],
                },
            ],
        });
        roundtrip(SessionEvent::ToolCallRequested {
            call: ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                arguments: r#"{"command":"ls"}"#.into(),
                risk: Some(Risk::Exec),
            },
        });
        roundtrip(SessionEvent::ToolCallFinished {
            call_id: "c1".into(),
            result: ToolResult {
                content: "ok".into(),
                is_error: false,
                details: Some(ToolResultDetails::Shell {
                    exit_code: Some(0),
                    signal: None,
                    timed_out: false,
                    cancelled: false,
                    truncated: false,
                    total_lines: 1,
                    total_bytes: 2,
                    full_output_path: None,
                    duration_ms: 5,
                }),
            },
        });
        roundtrip(SessionEvent::StateChanged {
            state: SessionState::WaitingLock,
        });
        roundtrip(SessionEvent::AssistantMessage {
            text: "done".into(),
            reasoning: Some("thought".into()),
            model: "openai/gpt".into(),
        });
    }
}
