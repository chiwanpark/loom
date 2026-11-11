pub mod api;
pub mod auth;
pub mod codec;
pub mod relay;
pub mod session;

pub use api::*;
pub use auth::*;
pub use codec::{CodecError, MAX_FRAME_LEN, decode, encode, undecodable_request_id};
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
        roundtrip(SessionEvent::ToolCallFinished {
            call_id: "c2".into(),
            result: ToolResult {
                content: "Wrote a.txt".into(),
                is_error: false,
                details: Some(ToolResultDetails::WriteWithDiff {
                    path: "a.txt".into(),
                    bytes: 3,
                    created: false,
                    diff: "-old\n+new\n".into(),
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
        roundtrip(SessionEvent::ModelChanged {
            model: "provider/model".into(),
            reasoning: Some(ReasoningEffort::High),
        });
        roundtrip(SessionEvent::ModelChanged {
            model: "provider/model".into(),
            reasoning: None,
        });
    }

    /// Events stored by an earlier agent still decode: these are the bytes
    /// protocol version 3 wrote for the last event it had.
    #[test]
    fn stored_events_from_before_model_changed_still_decode() {
        let stored = [7, 15, 3, 1];
        let event: SequencedEvent = decode(&stored).unwrap();
        assert_eq!(
            event,
            SequencedEvent {
                seq: 7,
                event: SessionEvent::ContextCompacted {
                    removed_messages: 3,
                    summarized: true,
                },
            }
        );
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
            Command::GetWorkspaceChanges,
            Command::GetFileDiff {
                path: "src/new.rs".into(),
                old_path: Some("src/old.rs".into()),
            },
            Command::RenameSession {
                session: session.clone(),
                title: Some("Renamed".into()),
            },
            Command::ListDirectory { path: "src".into() },
            Command::ReadFile {
                path: "src/main.rs".into(),
            },
            Command::AttachTerminal {
                terminal: 3,
                cols: 80,
                rows: 24,
            },
            Command::CompactSession {
                session: session.clone(),
            },
            Command::WriteFile {
                path: "src/main.rs".into(),
                text: "fn main() {}\n".into(),
                expected: FileVersion([7; 32]),
            },
            Command::ReadFileChunk {
                path: "data.bin".into(),
                offset: 1 << 40,
                len: MAX_FILE_CHUNK,
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
        message_roundtrip(ServerMessage::Response {
            id: 3,
            result: Ok(Payload::WorkspaceChanges(WorkspaceChanges {
                repository: true,
                branch: Some("main".into()),
                files: vec![FileChange {
                    path: "src/new.rs".into(),
                    old_path: Some("src/old.rs".into()),
                    status: ChangeStatus::Renamed,
                    added: 3,
                    removed: 1,
                    binary: false,
                }],
                truncated: false,
            })),
        });
        message_roundtrip(ServerMessage::Response {
            id: 5,
            result: Ok(Payload::Directory(DirectoryListing {
                path: "src".into(),
                entries: vec![DirEntry {
                    name: "main.rs".into(),
                    kind: EntryKind::File,
                    symlink: false,
                    size: 12,
                    ignored: false,
                }],
                truncated: false,
            })),
        });
        message_roundtrip(ServerMessage::Response {
            id: 6,
            result: Ok(Payload::FileContent(FileContent {
                path: "src/main.rs".into(),
                text: "fn main() {}\n".into(),
                size: 13,
                binary: false,
                truncated: false,
                valid_utf8: true,
                version: FileVersion([1; 32]),
            })),
        });
        message_roundtrip(ServerMessage::Response {
            id: 7,
            result: Ok(Payload::FileWritten(FileVersion([2; 32]))),
        });
        message_roundtrip(ServerMessage::Response {
            id: 9,
            result: Ok(Payload::FileChunk(FileChunk {
                size: 1 << 40,
                modified: Some(1_700_000_000_123_456_789),
                data: vec![0, 1, 255],
            })),
        });
        message_roundtrip(ServerMessage::Response {
            id: 8,
            result: Err(ApiError::new(ErrorCode::Conflict, "changed")),
        });
        message_roundtrip(ServerMessage::Response {
            id: 4,
            result: Ok(Payload::FileDiff(FileDiff {
                path: "a.txt".into(),
                diff: "@@ -1 +1 @@\n-a\n+b\n".into(),
                binary: false,
                truncated: true,
            })),
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
