pub mod api;
pub mod auth;
pub mod codec;
pub mod relay;
pub mod session;

pub use api::*;
pub use auth::*;
pub use codec::{CodecError, MAX_FRAME_LEN, decode, encode};
pub use session::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(event: SessionEvent) {
        let bytes = encode(&SequencedEvent {
            seq: 7,
            event: event.clone(),
        })
        .unwrap();
        let decoded: SequencedEvent = decode(&bytes).unwrap();
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
            model: "provider/model".into(),
        });
    }

    fn message_roundtrip<T>(message: T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let decoded: T = decode(&encode(&message).unwrap()).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn api_messages_roundtrip_through_postcard() {
        let session = SessionId("s1".into());
        message_roundtrip(ClientMessage::Hello(ClientHello {
            version: PROTOCOL_VERSION,
            client: "test".into(),
        }));
        for command in [
            Command::GetAgentInfo,
            Command::StartLogin {
                provider: "p".into(),
                method: LoginMethod::Browser {
                    redirect_uri: Some("http://127.0.0.1:1/cb".into()),
                    account: None,
                },
            },
            Command::CompleteLogin {
                login: "l".into(),
                input: LoginInput::Redirect { url: "u".into() },
            },
            Command::SetApiKey {
                provider: "p".into(),
                api_key: None,
            },
            Command::Subscribe {
                session: session.clone(),
                since_seq: Some(3),
            },
            Command::SendPrompt {
                session: session.clone(),
                content: vec![ContentPart::text("hi")],
            },
            Command::SetReasoning {
                session: session.clone(),
                effort: Some(ReasoningEffort::High),
            },
            Command::SetPolicy {
                session: session.clone(),
                default: Some(PolicyAction::Deny),
            },
        ] {
            message_roundtrip(ClientMessage::Request { id: 9, command });
        }
        let agent = AgentInfo {
            instance: InstanceId("abc".into()),
            workspace: "/w".into(),
            pid: 42,
            started_at: 1,
            version: "0.1.0".into(),
            default_model: None,
        };
        message_roundtrip(ServerMessage::Hello(ServerHello {
            version: PROTOCOL_VERSION,
            agent: agent.clone(),
        }));
        message_roundtrip(ServerMessage::Response {
            id: 1,
            result: Ok(Payload::Providers(vec![ProviderStatus {
                provider: "p".into(),
                display_name: "P".into(),
                enabled: true,
                login_methods: vec![
                    LoginMethodInfo::Browser {
                        redirect: RedirectSpec {
                            host: "127.0.0.1".into(),
                            port: None,
                            path: "/cb".into(),
                        },
                    },
                    LoginMethodInfo::ApiKey,
                ],
                api_key_set: false,
                accounts: vec![AccountStatus {
                    id: "a".into(),
                    label: None,
                    active: true,
                    signed_in: true,
                }],
            }])),
        });
        message_roundtrip(ServerMessage::Response {
            id: 2,
            result: Err(ApiError::new(ErrorCode::Busy, "busy")),
        });
        message_roundtrip(ServerMessage::Event {
            session,
            event: SequencedEvent {
                seq: 1,
                event: SessionEvent::TurnFinished {
                    outcome: TurnOutcome::Completed,
                },
            },
        });
        message_roundtrip(ServerMessage::LoginFinished {
            login: "l".into(),
            result: Ok(AccountInfo {
                provider: "p".into(),
                account_id: Some("a".into()),
                label: None,
            }),
        });
        message_roundtrip(ServerMessage::Notice(Notice::ConfigReloaded {
            restart_required: vec!["socket".into()],
        }));
    }

    #[test]
    fn rejects_garbage_and_oversized_frames() {
        assert!(decode::<ClientMessage>(&[0xff, 0xff, 0xff]).is_err());
        let big = vec![0u8; MAX_FRAME_LEN + 1];
        assert!(matches!(
            decode::<ClientMessage>(&big),
            Err(CodecError::TooLarge(_))
        ));
    }
}
