pub mod api;
pub mod auth;
pub mod codec;
pub mod push;
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

    // Stored events keep their states only while the positions stay.
    #[test]
    fn session_states_keep_their_positions() {
        let states = [
            SessionState::Idle,
            SessionState::Thinking,
            SessionState::RunningTool,
            SessionState::WaitingApproval,
            SessionState::Responding,
            SessionState::Completed,
        ];
        for (position, state) in states.into_iter().enumerate() {
            assert_eq!(encode(&state).unwrap(), vec![position as u8], "{state:?}");
        }
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
            state: SessionState::Responding,
        });
        roundtrip(SessionEvent::StateChanged {
            state: SessionState::Completed,
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
        roundtrip(SessionEvent::Cleared);
        roundtrip(SessionEvent::Rewound { seq: 12 });
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

    #[test]
    fn usage_so_far_follows_compaction_and_clearing() {
        let usage = Usage {
            input: 10,
            output: 2,
            total: 12,
            ..Usage::default()
        };
        let mut so_far = UsageSoFar::default();
        so_far.apply(&SessionEvent::Usage { usage });
        so_far.apply(&SessionEvent::Usage { usage });
        assert_eq!(so_far.total.input, 20);
        assert_eq!(so_far.total.total, 24);
        assert_eq!(so_far.last, Some(usage));
        so_far.apply(&SessionEvent::ContextCompacted {
            removed_messages: 1,
            summarized: false,
        });
        assert_eq!(so_far.last, None);
        assert_eq!(so_far.total.output, 4);
        so_far.apply(&SessionEvent::Cleared);
        assert_eq!(so_far, UsageSoFar::default());
    }

    #[test]
    fn a_pending_message_added_again_replaces_the_one_it_repeats() {
        let message = |id: u64, text: &str| PendingMessage {
            id,
            content: vec![ContentPart::text(text)],
            delivery: Delivery::Queue,
        };
        let mut pending = Vec::new();
        for event in [
            SessionEvent::PendingAdded {
                message: message(1, "a"),
            },
            SessionEvent::PendingAdded {
                message: message(2, "b"),
            },
            SessionEvent::PendingAdded {
                message: message(1, "a"),
            },
        ] {
            apply_pending(&mut pending, &event);
        }
        assert_eq!(pending, vec![message(1, "a"), message(2, "b")]);
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
                tail: Some(50),
            },
            Command::GetHistory {
                session: session.clone(),
                before_seq: 40,
                items: 50,
            },
            Command::SendPrompt {
                session: session.clone(),
                content: vec![ContentPart::text("hi")],
                delivery: Delivery::Steer,
            },
            Command::CancelPending {
                session: session.clone(),
                id: 3,
            },
            Command::SetReasoning {
                session: session.clone(),
                effort: Some(ReasoningEffort::High),
            },
            Command::SetPolicy {
                session: session.clone(),
                default: Some(PolicyAction::Deny),
            },
            Command::GetWorkspaceChanges { session: None },
            Command::GetFileDiff {
                path: "src/new.rs".into(),
                old_path: Some("src/old.rs".into()),
                session: Some(session.clone()),
                committed: true,
            },
            Command::RenameSession {
                session: session.clone(),
                title: Some("Renamed".into()),
            },
            Command::ListDirectory {
                path: "src".into(),
                session: None,
            },
            Command::ReadFile {
                path: "src/main.rs".into(),
                session: Some(session.clone()),
            },
            Command::AttachTerminal {
                terminal: 3,
                cols: 80,
                rows: 24,
                owner: 9,
                take_over: true,
            },
            Command::ListTerminals,
            Command::ListPorts,
            Command::OpenTunnel {
                tunnel: 4,
                host: "localhost".into(),
                port: 3000,
            },
            Command::TunnelData {
                tunnel: 4,
                data: b"GET / HTTP/1.1\r\n\r\n".to_vec(),
            },
            Command::TunnelAck {
                tunnel: 4,
                bytes: 4096,
            },
            Command::TunnelEnd { tunnel: 4 },
            Command::CloseTunnel { tunnel: 4 },
            Command::CompactSession {
                session: session.clone(),
            },
            Command::WriteFile {
                path: "src/main.rs".into(),
                text: "fn main() {}\n".into(),
                expected: FileVersion([7; 32]),
                session: None,
            },
            Command::ReadFileChunk {
                path: "data.bin".into(),
                offset: 1 << 40,
                len: MAX_FILE_CHUNK,
                session: None,
            },
            Command::ClearSession {
                session: session.clone(),
            },
            Command::ReloadConfig,
            Command::GetNotes,
            Command::WriteNotes {
                text: "- plan\n".into(),
                expected: FileVersion([3; 32]),
            },
            Command::GetTasks {
                session: session.clone(),
            },
            Command::SetPushToken {
                token: Some("fcm-token".into()),
            },
            Command::SetPushToken { token: None },
            Command::MarkSessionSeen {
                session: session.clone(),
            },
            Command::UploadChunk {
                upload: 5,
                path: "assets/logo.png".into(),
                offset: 1 << 20,
                data: vec![0x89, b'P', b'N', b'G'],
                session: Some(session.clone()),
            },
            Command::FinishUpload {
                upload: 5,
                size: (1 << 20) + 4,
                overwrite: true,
                session: Some(session.clone()),
            },
            Command::CancelUpload { upload: 5 },
            Command::GetSystemPrompt {
                session: session.clone(),
            },
            Command::UpdateAgent { version: None },
            Command::UpdateAgent {
                version: Some("0.4.0".into()),
            },
            Command::UpdateTasks {
                session: session.clone(),
                changes: vec![
                    TaskChange::Add {
                        text: "write tests".into(),
                        status: TaskStatus::InProgress,
                    },
                    TaskChange::Update {
                        id: 2,
                        text: None,
                        status: Some(TaskStatus::Done),
                    },
                    TaskChange::Remove { id: 1 },
                ],
            },
            Command::Rewind {
                session: session.clone(),
                seq: 12,
            },
            Command::CreateSession {
                model: None,
                worktree: Some(WorktreeOptions {
                    branch: Some("loom/x".into()),
                    base: None,
                }),
            },
            Command::GetWorktreeStatus {
                session: session.clone(),
            },
            Command::MergeWorktree {
                session: session.clone(),
                strategy: MergeStrategy::Squash {
                    message: "Add x".into(),
                },
                remove: true,
            },
            Command::OpenPullRequest {
                session: session.clone(),
                title: "Add x".into(),
                body: String::new(),
                draft: true,
            },
            Command::RemoveWorktree {
                session: session.clone(),
                force: false,
            },
            Command::SetCommentGuard {
                session: session.clone(),
                enabled: Some(false),
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
                committed: Some(CommittedChanges {
                    base: "origin/main".into(),
                    since: "0123abcd".into(),
                    commits: vec![CommitInfo {
                        id: "4567cdef".into(),
                        summary: "Add new.rs".into(),
                        author: "Ada".into(),
                        time: 1_700_000_000,
                    }],
                    files: vec![FileChange {
                        path: "src/new.rs".into(),
                        old_path: None,
                        status: ChangeStatus::Added,
                        added: 3,
                        removed: 0,
                        binary: false,
                    }],
                    truncated: false,
                }),
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
        message_roundtrip(ServerMessage::Events {
            session: session.clone(),
            events: vec![SequencedEvent {
                seq: 1,
                event: SessionEvent::TurnFinished {
                    outcome: TurnOutcome::Completed,
                },
            }],
            window: Some(EventWindow {
                has_older: true,
                usage: UsageSoFar {
                    total: Usage {
                        input: 3,
                        ..Usage::default()
                    },
                    last: None,
                },
                pending: vec![PendingMessage {
                    id: 2,
                    content: vec![ContentPart::text("next")],
                    delivery: Delivery::Queue,
                }],
            }),
        });
        message_roundtrip(ServerMessage::Events {
            session,
            events: Vec::new(),
            window: None,
        });
        message_roundtrip(ServerMessage::Response {
            id: 9,
            result: Ok(Payload::History(History {
                events: vec![SequencedEvent {
                    seq: 2,
                    event: SessionEvent::Cleared,
                }],
                has_older: false,
                usage: UsageSoFar::default(),
            })),
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
        message_roundtrip(ServerMessage::Response {
            id: 10,
            result: Ok(Payload::Notes(NotesContent {
                text: "- plan\n".into(),
                version: FileVersion([4; 32]),
                max_bytes: 65536,
            })),
        });
        message_roundtrip(ServerMessage::Response {
            id: 11,
            result: Ok(Payload::NotesWritten(FileVersion([5; 32]))),
        });
        message_roundtrip(ServerMessage::Response {
            id: 12,
            result: Ok(Payload::SystemPrompt(SystemPrompt {
                text: "You are Loom.\n\n# Environment".into(),
                warnings: vec!["AGENTS.md was truncated".into()],
                tools: vec![ToolDefinition {
                    name: "read".into(),
                    description: "Read a file.".into(),
                    parameters: "{\n  \"type\": \"object\"\n}".into(),
                }],
            })),
        });
        let agent = AgentInfo {
            instance: InstanceId("a1".into()),
            workspace: "/work".into(),
            pid: 42,
            started_at: 1,
            version: "0.3.0 (abc 2026-11-16)".into(),
            default_model: None,
        };
        message_roundtrip(ServerMessage::Response {
            id: 14,
            result: Ok(Payload::AgentUpdated(AgentUpdate {
                previous: "loom-agent 0.3.0 (abc 2026-11-16)".into(),
                version: "loom-agent 0.4.0 (def 2026-12-01)".into(),
                path: "/home/me/.local/bin/loom-agent".into(),
                restarting: vec![agent.clone()],
                not_restarted: vec![AgentInfo {
                    pid: 43,
                    version: "0.2.0 (old 2026-11-01)".into(),
                    ..agent
                }],
            })),
        });
        message_roundtrip(ServerMessage::Response {
            id: 15,
            result: Ok(Payload::Rewound(vec![ContentPart::text("try again")])),
        });
        for reload in [
            ConfigReload::Applied {
                restart_required: vec!["socket".into()],
            },
            ConfigReload::Unchanged,
        ] {
            message_roundtrip(ServerMessage::Response {
                id: 13,
                result: Ok(Payload::ConfigReloaded(reload)),
            });
        }
        message_roundtrip(ServerMessage::Notice(Notice::NotesChanged {
            version: FileVersion([6; 32]),
        }));
        let tasks = vec![Task {
            id: 3,
            text: "ship it".into(),
            status: TaskStatus::Created,
        }];
        message_roundtrip(ServerMessage::Response {
            id: 12,
            result: Ok(Payload::Tasks(TasksContent {
                tasks: tasks.clone(),
                max_tasks: 100,
            })),
        });
        message_roundtrip(ServerMessage::Notice(Notice::TasksChanged {
            session: SessionId("s2".into()),
            tasks,
        }));
    }

    #[test]
    fn terminal_lists_roundtrip() {
        let terminals = vec![
            TerminalInfo {
                id: 1,
                holder: TerminalHolder::You,
            },
            TerminalInfo {
                id: 2,
                holder: TerminalHolder::Other(TerminalClient {
                    program: "loom-android".into(),
                    label: Some("phone".into()),
                }),
            },
            TerminalInfo {
                id: 3,
                holder: TerminalHolder::Nobody,
            },
        ];
        message_roundtrip(ServerMessage::Response {
            id: 13,
            result: Ok(Payload::Terminals(terminals.clone())),
        });
        message_roundtrip(ServerMessage::Notice(Notice::TerminalsChanged(terminals)));
        let client = |program: &str, label: Option<&str>| TerminalClient {
            program: program.into(),
            label: label.map(Into::into),
        };
        assert_eq!(client("loom-android", Some("phone")).describe(), "phone");
        assert_eq!(client("loom-gui", None).describe(), "the desktop app");
        assert_eq!(
            client("loom-android", Some("")).describe(),
            "the Android app"
        );
        assert_eq!(client("other", None).describe(), "other");
    }

    #[test]
    fn ports_and_tunnels_roundtrip() {
        let port = |address: &str| ListeningPort {
            port: 3000,
            address: address.into(),
            process: Some("node".into()),
            pid: Some(42),
        };
        message_roundtrip(ServerMessage::Response {
            id: 1,
            result: Ok(Payload::Ports(vec![port("127.0.0.1"), port("::")])),
        });
        message_roundtrip(ServerMessage::TunnelData {
            tunnel: 4,
            data: vec![0, 1, 2],
        });
        message_roundtrip(ServerMessage::TunnelEnd {
            tunnel: 4,
            error: Some("reset".into()),
        });
        assert_eq!(port("0.0.0.0").connect_host(), "127.0.0.1");
        assert_eq!(port("::").connect_host(), "::1");
        assert_eq!(port("192.168.0.2").connect_host(), "192.168.0.2");
        // A tunnel's data with its message's other fields fits in one
        // message of the encrypted transport.
        let data = encode(&ClientMessage::Request {
            id: u64::MAX,
            command: Command::TunnelData {
                tunnel: u64::MAX,
                data: vec![0xff; MAX_TUNNEL_DATA as usize],
            },
        })
        .unwrap();
        assert!(data.len() < 65_535 - 16, "{}", data.len());
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
