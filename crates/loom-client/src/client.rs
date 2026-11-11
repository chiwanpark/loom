use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use loom_proto::{
    AccountInfo, AgentInfo, ApiError, ClientHello, ClientMessage, Command, ContentPart, Decision,
    LoginId, LoginInput, LoginMethod, LoginStarted, ModelInfo, Notice, PROTOCOL_VERSION, Payload,
    PolicyAction, ProviderStatus, ReasoningEffort, RequestId, SequencedEvent, ServerMessage,
    SessionId, SessionInfo, TerminalId, UsageLimits, decode, encode,
};
use loom_transport::{Diagnostics, FrameReceiver, FrameSender, FrameStream};
use tokio::sync::{mpsc, oneshot};

use crate::error::{ClientError, Result};

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    Event {
        session: SessionId,
        event: SequencedEvent,
    },
    LoginFinished {
        login: LoginId,
        result: std::result::Result<AccountInfo, ApiError>,
    },
    Notice(Notice),
    TerminalOutput {
        terminal: TerminalId,
        data: Vec<u8>,
    },
    TerminalExited {
        terminal: TerminalId,
        status: i32,
    },
}

type Pending = Arc<Mutex<Option<HashMap<RequestId, oneshot::Sender<Result<Payload>>>>>>;

#[derive(Clone)]
pub struct Client {
    agent: AgentInfo,
    writer: mpsc::Sender<ClientMessage>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

pub struct Notifications {
    rx: mpsc::UnboundedReceiver<Notification>,
}

impl Notifications {
    pub async fn recv(&mut self) -> Option<Notification> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Notification> {
        self.rx.try_recv().ok()
    }
}

fn with_diagnostics(err: ClientError, diagnostics: &Option<Diagnostics>) -> ClientError {
    match diagnostics.as_ref().map(|d| d.text()) {
        Some(text) if !text.trim().is_empty() => ClientError::Connect {
            message: err.to_string(),
            diagnostics: text.trim().to_owned(),
        },
        _ => err,
    }
}

async fn send_message(sender: &mut FrameSender, message: &ClientMessage) -> Result<()> {
    sender.send(Bytes::from(encode(message)?)).await?;
    Ok(())
}

async fn recv_message(receiver: &mut FrameReceiver) -> Result<Option<ServerMessage>> {
    match receiver.recv().await? {
        Some(frame) => Ok(Some(decode(&frame)?)),
        None => Ok(None),
    }
}

impl Client {
    pub async fn connect(stream: FrameStream, client_name: &str) -> Result<(Self, Notifications)> {
        let diagnostics = stream.diagnostics();
        let (mut sender, mut receiver, _) = stream.split();
        let handshake = async {
            send_message(
                &mut sender,
                &ClientMessage::Hello(ClientHello {
                    version: PROTOCOL_VERSION,
                    client: client_name.to_owned(),
                }),
            )
            .await?;
            match recv_message(&mut receiver).await? {
                Some(ServerMessage::Hello(hello)) => Ok(hello.agent),
                Some(ServerMessage::Rejected(err)) => Err(ClientError::Rejected(err)),
                Some(other) => Err(ClientError::Protocol(format!(
                    "expected Hello, got {other:?}"
                ))),
                None => Err(ClientError::Closed),
            }
        };
        let agent = match tokio::time::timeout(HELLO_TIMEOUT, handshake).await {
            Ok(Ok(agent)) => agent,
            Ok(Err(err)) => return Err(with_diagnostics(err, &diagnostics)),
            Err(_) => {
                return Err(with_diagnostics(
                    ClientError::Protocol("timed out waiting for the agent".into()),
                    &diagnostics,
                ));
            }
        };

        let (writer, mut outgoing) = mpsc::channel::<ClientMessage>(256);
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if let Err(err) = send_message(&mut sender, &message).await {
                    tracing::debug!(error = %err, "connection write failed");
                    break;
                }
            }
            let _ = sender.close().await;
        });

        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (notify, rx) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            loop {
                let message = match recv_message(&mut receiver).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(error = %err, "connection read failed");
                        break;
                    }
                };
                let notification = match message {
                    ServerMessage::Response { id, result } => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_mut()
                            .and_then(|map| map.remove(&id));
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(result.map_err(ClientError::Api));
                        }
                        continue;
                    }
                    ServerMessage::Event { session, event } => {
                        Notification::Event { session, event }
                    }
                    ServerMessage::LoginFinished { login, result } => {
                        Notification::LoginFinished { login, result }
                    }
                    ServerMessage::Notice(notice) => Notification::Notice(notice),
                    ServerMessage::TerminalOutput { terminal, data } => {
                        Notification::TerminalOutput { terminal, data }
                    }
                    ServerMessage::TerminalExited { terminal, status } => {
                        Notification::TerminalExited { terminal, status }
                    }
                    ServerMessage::Rejected(err) => {
                        tracing::warn!(error = %err, "agent closed the connection");
                        break;
                    }
                    ServerMessage::Hello(_) => continue,
                };
                let _ = notify.send(notification);
            }
            reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        });

        Ok((
            Self {
                agent,
                writer,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            Notifications { rx },
        ))
    }

    pub fn agent(&self) -> &AgentInfo {
        &self.agent
    }

    pub fn is_closed(&self) -> bool {
        self.writer.is_closed()
            || self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
    }

    pub async fn request(&self, command: Command) -> Result<Payload> {
        self.submit(command).await?.await
    }

    pub async fn submit(
        &self,
        command: Command,
    ) -> Result<impl std::future::Future<Output = Result<Payload>> + Send + 'static> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            Some(map) => map.insert(id, tx),
            None => return Err(ClientError::Closed),
        };
        if self
            .writer
            .send(ClientMessage::Request { id, command })
            .await
            .is_err()
        {
            return Err(ClientError::Closed);
        }
        Ok(async move { rx.await.unwrap_or(Err(ClientError::Closed)) })
    }

    pub async fn agent_info(&self) -> Result<AgentInfo> {
        match self.request(Command::GetAgentInfo).await? {
            Payload::AgentInfo(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub async fn list_providers(&self) -> Result<Vec<ProviderStatus>> {
        match self.request(Command::ListProviders).await? {
            Payload::Providers(list) => Ok(list),
            other => Err(unexpected(other)),
        }
    }

    pub async fn usage_limits(&self, provider: &str) -> Result<Option<UsageLimits>> {
        match self
            .request(Command::GetUsageLimits {
                provider: provider.to_owned(),
            })
            .await?
        {
            Payload::UsageLimits(limits) => Ok(limits),
            other => Err(unexpected(other)),
        }
    }

    pub async fn telemetry_status(&self) -> Result<loom_proto::TelemetryStatus> {
        match self.request(Command::GetTelemetryStatus).await? {
            Payload::Telemetry(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    pub async fn list_models(&self, provider: &str) -> Result<Vec<ModelInfo>> {
        match self
            .request(Command::ListModels {
                provider: provider.to_owned(),
            })
            .await?
        {
            Payload::Models(list) => Ok(list),
            other => Err(unexpected(other)),
        }
    }

    pub async fn start_login(&self, provider: &str, method: LoginMethod) -> Result<LoginStarted> {
        match self
            .request(Command::StartLogin {
                provider: provider.to_owned(),
                method,
            })
            .await?
        {
            Payload::LoginStarted(started) => Ok(started),
            other => Err(unexpected(other)),
        }
    }

    pub async fn complete_login(&self, login: &str, input: LoginInput) -> Result<AccountInfo> {
        match self
            .request(Command::CompleteLogin {
                login: login.to_owned(),
                input,
            })
            .await?
        {
            Payload::Account(account) => Ok(account),
            other => Err(unexpected(other)),
        }
    }

    pub async fn cancel_login(&self, login: &str) -> Result<()> {
        self.ok(Command::CancelLogin {
            login: login.to_owned(),
        })
        .await
    }

    pub async fn set_api_key(&self, provider: &str, api_key: Option<String>) -> Result<()> {
        self.ok(Command::SetApiKey {
            provider: provider.to_owned(),
            api_key,
        })
        .await
    }

    pub async fn select_account(&self, provider: &str, account: &str) -> Result<()> {
        self.ok(Command::SelectAccount {
            provider: provider.to_owned(),
            account: account.to_owned(),
        })
        .await
    }

    pub async fn logout(&self, provider: &str) -> Result<()> {
        self.ok(Command::Logout {
            provider: provider.to_owned(),
        })
        .await
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        match self.request(Command::ListSessions).await? {
            Payload::Sessions(list) => Ok(list),
            other => Err(unexpected(other)),
        }
    }

    pub async fn create_session(&self, model: Option<&str>) -> Result<SessionInfo> {
        self.session(Command::CreateSession {
            model: model.map(str::to_owned),
        })
        .await
    }

    pub async fn subscribe(
        &self,
        session: &SessionId,
        since_seq: Option<u64>,
    ) -> Result<SessionInfo> {
        self.session(Command::Subscribe {
            session: session.clone(),
            since_seq,
        })
        .await
    }

    pub async fn unsubscribe(&self, session: &SessionId) -> Result<()> {
        self.ok(Command::Unsubscribe {
            session: session.clone(),
        })
        .await
    }

    pub async fn send_prompt(&self, session: &SessionId, content: Vec<ContentPart>) -> Result<()> {
        self.ok(Command::SendPrompt {
            session: session.clone(),
            content,
        })
        .await
    }

    pub async fn send_text(&self, session: &SessionId, text: &str) -> Result<()> {
        self.send_prompt(session, vec![ContentPart::text(text)])
            .await
    }

    pub async fn interrupt(&self, session: &SessionId) -> Result<bool> {
        match self
            .request(Command::Interrupt {
                session: session.clone(),
            })
            .await?
        {
            Payload::Interrupted(interrupted) => Ok(interrupted),
            other => Err(unexpected(other)),
        }
    }

    pub async fn resolve_approval(
        &self,
        session: &SessionId,
        call_id: &str,
        decision: Decision,
    ) -> Result<()> {
        self.ok(Command::ResolveApproval {
            session: session.clone(),
            call_id: call_id.to_owned(),
            decision,
        })
        .await
    }

    pub async fn set_model(&self, session: &SessionId, model: Option<&str>) -> Result<SessionInfo> {
        self.session(Command::SetModel {
            session: session.clone(),
            model: model.map(str::to_owned),
        })
        .await
    }

    pub async fn delete_session(&self, session: &SessionId) -> Result<()> {
        self.ok(Command::DeleteSession {
            session: session.clone(),
        })
        .await
    }

    pub async fn archive_session(
        &self,
        session: &SessionId,
        archived: bool,
    ) -> Result<SessionInfo> {
        self.session(Command::ArchiveSession {
            session: session.clone(),
            archived,
        })
        .await
    }

    pub async fn set_reasoning(
        &self,
        session: &SessionId,
        effort: Option<ReasoningEffort>,
    ) -> Result<SessionInfo> {
        self.session(Command::SetReasoning {
            session: session.clone(),
            effort,
        })
        .await
    }

    pub async fn set_policy(
        &self,
        session: &SessionId,
        default: Option<PolicyAction>,
    ) -> Result<SessionInfo> {
        self.session(Command::SetPolicy {
            session: session.clone(),
            default,
        })
        .await
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        match self.request(Command::ListAgents).await? {
            Payload::Agents(list) => Ok(list),
            other => Err(unexpected(other)),
        }
    }

    pub async fn spawn_agent(&self, workspace: impl Into<std::path::PathBuf>) -> Result<AgentInfo> {
        match self
            .request(Command::SpawnAgent {
                workspace: workspace.into(),
            })
            .await?
        {
            Payload::AgentInfo(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub async fn stop_agent(&self, instance: &loom_proto::InstanceId) -> Result<()> {
        self.ok(Command::StopAgent {
            instance: instance.clone(),
        })
        .await
    }

    pub async fn metrics(&self) -> Result<loom_proto::AgentMetrics> {
        match self.request(Command::GetMetrics).await? {
            Payload::Metrics(metrics) => Ok(metrics),
            other => Err(unexpected(other)),
        }
    }

    pub async fn rename_session(
        &self,
        session: &SessionId,
        title: Option<String>,
    ) -> Result<SessionInfo> {
        self.session(Command::RenameSession {
            session: session.clone(),
            title,
        })
        .await
    }

    pub async fn list_directory(&self, path: &str) -> Result<loom_proto::DirectoryListing> {
        let command = Command::ListDirectory {
            path: path.to_string(),
        };
        match self.request(command).await? {
            Payload::Directory(listing) => Ok(listing),
            other => Err(unexpected(other)),
        }
    }

    pub async fn read_file(&self, path: &str) -> Result<loom_proto::FileContent> {
        let command = Command::ReadFile {
            path: path.to_string(),
        };
        match self.request(command).await? {
            Payload::FileContent(content) => Ok(content),
            other => Err(unexpected(other)),
        }
    }

    /// Replaces the text of a file that still has the `expected` version, and
    /// returns its new version.
    pub async fn write_file(
        &self,
        path: &str,
        text: String,
        expected: loom_proto::FileVersion,
    ) -> Result<loom_proto::FileVersion> {
        let command = Command::WriteFile {
            path: path.to_string(),
            text,
            expected,
        };
        match self.request(command).await? {
            Payload::FileWritten(version) => Ok(version),
            other => Err(unexpected(other)),
        }
    }

    pub async fn workspace_changes(&self) -> Result<loom_proto::WorkspaceChanges> {
        match self.request(Command::GetWorkspaceChanges).await? {
            Payload::WorkspaceChanges(changes) => Ok(changes),
            other => Err(unexpected(other)),
        }
    }

    pub async fn file_diff(
        &self,
        path: &str,
        old_path: Option<&str>,
    ) -> Result<loom_proto::FileDiff> {
        let command = Command::GetFileDiff {
            path: path.to_string(),
            old_path: old_path.map(str::to_string),
        };
        match self.request(command).await? {
            Payload::FileDiff(diff) => Ok(diff),
            other => Err(unexpected(other)),
        }
    }

    pub async fn host_key(&self) -> Result<loom_proto::HostKeyInfo> {
        match self.request(Command::GetHostKey).await? {
            Payload::HostKey(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub async fn authorize_client(
        &self,
        pubkey: [u8; 32],
        label: impl Into<String>,
    ) -> Result<loom_proto::HostKeyInfo> {
        match self
            .request(Command::AuthorizeClient {
                pubkey,
                label: label.into(),
            })
            .await?
        {
            Payload::HostKey(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub async fn list_clients(&self) -> Result<Vec<loom_proto::AuthorizedClient>> {
        match self.request(Command::ListClients).await? {
            Payload::Clients(list) => Ok(list),
            other => Err(unexpected(other)),
        }
    }

    pub async fn revoke_client(&self, label: impl Into<String>) -> Result<()> {
        self.ok(Command::RevokeClient {
            label: label.into(),
        })
        .await
    }

    pub async fn open_terminal(&self, cols: u16, rows: u16) -> Result<TerminalId> {
        match self.request(Command::OpenTerminal { cols, rows }).await? {
            Payload::Terminal(id) => Ok(id),
            other => Err(unexpected(other)),
        }
    }

    pub async fn terminal_input(&self, terminal: TerminalId, data: Vec<u8>) -> Result<()> {
        self.ok(Command::TerminalInput { terminal, data }).await
    }

    pub async fn resize_terminal(&self, terminal: TerminalId, cols: u16, rows: u16) -> Result<()> {
        self.ok(Command::ResizeTerminal {
            terminal,
            cols,
            rows,
        })
        .await
    }

    pub async fn close_terminal(&self, terminal: TerminalId) -> Result<()> {
        self.ok(Command::CloseTerminal { terminal }).await
    }

    /// Starts compacting a session's context. Progress and the result arrive as
    /// session events (`StateChanged`, `ContextCompacted`, or a `Warning`).
    pub async fn compact_session(&self, session: &SessionId) -> Result<()> {
        self.ok(Command::CompactSession {
            session: session.clone(),
        })
        .await
    }

    /// Starts a session's conversation over, keeping its settings. Subscribers
    /// get a `Cleared` event. Fails with `Busy` while a turn is running.
    pub async fn clear_session(&self, session: &SessionId) -> Result<()> {
        self.ok(Command::ClearSession {
            session: session.clone(),
        })
        .await
    }

    /// Reattaches to a terminal opened over an earlier connection to the same agent.
    pub async fn attach_terminal(&self, terminal: TerminalId, cols: u16, rows: u16) -> Result<()> {
        self.ok(Command::AttachTerminal {
            terminal,
            cols,
            rows,
        })
        .await
    }

    async fn ok(&self, command: Command) -> Result<()> {
        match self.request(command).await? {
            Payload::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    async fn session(&self, command: Command) -> Result<SessionInfo> {
        match self.request(command).await? {
            Payload::Session(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }
}

pub(crate) fn unexpected(payload: Payload) -> ClientError {
    ClientError::Protocol(format!("unexpected response {payload:?}"))
}
