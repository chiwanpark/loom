use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use loom_llm::ProviderId;
use loom_proto::{
    AgentInfo, ApiError, ClientHello, ClientMessage, Command, ErrorCode, FileDiff, LoginPrompt,
    ModelInfo, Notice, PROTOCOL_VERSION, Payload, RequestId, SequencedEvent, ServerHello,
    ServerMessage, SessionId, TerminalClient, TerminalOwner,
};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentNotice};
use crate::convert;
use crate::events::Subscription;
use crate::repositories::Repositories;
use crate::session::Session;
use crate::terminals::{ConnectionId, Viewer, interactive_shell};
use crate::tunnels::Tunnels;
use crate::uploads::Uploads;

pub type Reply = Result<Payload, ApiError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Close,
}

pub struct Connection {
    agent: Agent,
    info: AgentInfo,
    out: mpsc::Sender<ServerMessage>,
    greeted: bool,
    subscriptions: HashMap<SessionId, JoinHandle<()>>,
    logins: Arc<Mutex<HashMap<String, CancellationToken>>>,
    tasks: Vec<JoinHandle<()>>,
    repositories: Option<Arc<dyn Repositories>>,
    connection_id: ConnectionId,
    /// The label of a client that comes through a relay, to tell people where
    /// its terminals are open.
    client_label: Option<String>,
    /// The name the client gave in its hello.
    program: String,
    tunnels: Tunnels,
    uploads: Arc<Uploads>,
}

fn protocol_error(message: &str) -> ApiError {
    ApiError::new(ErrorCode::Protocol, message)
}

impl Connection {
    pub fn new(agent: Agent, info: AgentInfo, out: mpsc::Sender<ServerMessage>) -> Self {
        agent.metrics().connection_opened();
        let connection_id = agent.terminals().connection_id();
        Self {
            agent,
            info,
            tunnels: Tunnels::new(out.clone()),
            out,
            greeted: false,
            subscriptions: HashMap::new(),
            logins: Arc::default(),
            tasks: Vec::new(),
            repositories: None,
            connection_id,
            client_label: None,
            program: String::new(),
            uploads: Arc::default(),
        }
    }

    /// The label a client that comes through a relay was paired with, which
    /// other clients see on the terminals it holds.
    pub fn with_client_label(mut self, label: Option<String>) -> Self {
        self.client_label = label;
        self
    }

    pub fn with_repositories(mut self, repositories: Option<Arc<dyn Repositories>>) -> Self {
        self.repositories = repositories;
        self
    }

    pub fn agent_info(&self) -> AgentInfo {
        AgentInfo {
            default_model: self.agent.config().llm.default_model.clone(),
            ..self.info.clone()
        }
    }

    pub async fn handle(&mut self, message: ClientMessage) -> Flow {
        self.tasks.retain(|t| !t.is_finished());
        match message {
            ClientMessage::Hello(hello) => self.hello(hello).await,
            ClientMessage::Request { .. } if !self.greeted => {
                self.send(ServerMessage::Rejected(protocol_error(
                    "the first message must be Hello",
                )))
                .await;
                Flow::Close
            }
            ClientMessage::Request { id, command } => {
                self.request(id, command).await;
                Flow::Continue
            }
        }
    }

    pub async fn unsupported_request(&self, id: RequestId) -> bool {
        if !self.greeted {
            return false;
        }
        self.respond(
            id,
            Err(ApiError::new(
                ErrorCode::Unsupported,
                "this agent does not support the request; update the agent",
            )),
        )
        .await;
        true
    }

    pub async fn reject(&self, error: ApiError) {
        self.send(ServerMessage::Rejected(error)).await;
    }

    async fn send(&self, message: ServerMessage) -> bool {
        self.out.send(message).await.is_ok()
    }

    async fn hello(&mut self, hello: ClientHello) -> Flow {
        if self.greeted {
            self.reject(protocol_error("Hello was already received"))
                .await;
            return Flow::Close;
        }
        if hello.version != PROTOCOL_VERSION {
            self.reject(ApiError::new(
                ErrorCode::VersionMismatch,
                format!(
                    "client speaks protocol {}, agent speaks {PROTOCOL_VERSION}",
                    hello.version
                ),
            ))
            .await;
            return Flow::Close;
        }
        self.greeted = true;
        tracing::debug!(client = %hello.client, "client connected");
        self.program = hello.client;
        let ok = self
            .send(ServerMessage::Hello(ServerHello {
                version: PROTOCOL_VERSION,
                agent: self.agent_info(),
            }))
            .await;
        if ok {
            self.tasks.push(tokio::spawn(forward_notices(
                self.agent.notices(),
                self.out.clone(),
            )));
            self.tasks.push(tokio::spawn(forward_terminals(
                self.agent.clone(),
                self.connection_id,
                self.out.clone(),
            )));
        }
        Flow::Continue
    }

    /// This connection, as a terminal's output goes to it.
    fn viewer(&self, owner: TerminalOwner) -> Viewer {
        Viewer {
            connection: self.connection_id,
            owner,
            client: TerminalClient {
                program: self.program.clone(),
                label: self.client_label.clone(),
            },
            out: self.out.clone(),
        }
    }

    async fn respond(&self, id: RequestId, result: Reply) {
        self.send(ServerMessage::Response { id, result }).await;
    }

    /// Runs `work` in the background and answers the request with
    /// [`Payload::Ok`] or its error when it is done.
    fn reply_later(
        &mut self,
        id: RequestId,
        work: impl Future<Output = Result<(), ApiError>> + Send + 'static,
    ) {
        self.spawn_reply(id, async move { work.await.map(|()| Payload::Ok) });
    }

    /// Runs `work` in the background and answers the request with its result.
    fn spawn_reply(&mut self, id: RequestId, work: impl Future<Output = Reply> + Send + 'static) {
        let out = self.out.clone();
        self.tasks.push(tokio::spawn(async move {
            let result = work.await;
            let _ = out.send(ServerMessage::Response { id, result }).await;
        }));
    }

    async fn request(&mut self, id: RequestId, command: Command) {
        match command {
            Command::ReloadConfig => {
                let agent = self.agent.clone();
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = agent
                        .reload_config()
                        .await
                        .map(Payload::ConfigReloaded)
                        .map_err(|e| convert::api_error(&e));
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::ListRepositories
            | Command::PushRepositories(_)
            | Command::OpenPullRequests { .. } => {
                let repositories = self.repositories.clone();
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = manage_repositories(repositories.as_deref(), command).await;
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::DeleteSession { session } => {
                let agent = self.agent.clone();
                self.spawn_reply(id, async move {
                    agent
                        .delete_session(&session)
                        .map(|()| Payload::Ok)
                        .map_err(|e| convert::api_error(&e))
                });
            }
            Command::GetWorkspaceChanges
            | Command::GetFileDiff { .. }
            | Command::ListDirectory { .. }
            | Command::ReadFile { .. }
            | Command::ReadFileChunk { .. }
            | Command::WriteFile { .. } => {
                let workspace = self.agent.workspace().clone();
                let root = workspace.root().to_path_buf();
                let codespace = self.repositories.is_some();
                self.spawn_reply(id, async move {
                    let base = crate::git::CommitBase::default();
                    let nested = match &command {
                        Command::GetWorkspaceChanges | Command::GetFileDiff { .. } if codespace => {
                            crate::git::nested_repositories(&root).await
                        }
                        _ => Vec::new(),
                    };
                    match command {
                        Command::GetFileDiff {
                            path,
                            old_path,
                            committed,
                            ..
                        } => match crate::git::split_nested(&nested, &path, old_path.as_deref()) {
                            Some((directory, inner, old)) => crate::git::file_diff(
                                &root.join(directory),
                                inner,
                                old,
                                committed.then_some(crate::git::CommitBase::default()),
                            )
                            .await
                            .map(|diff| Payload::FileDiff(FileDiff { path, ..diff })),
                            None => crate::git::file_diff(
                                &root,
                                &path,
                                old_path.as_deref(),
                                committed.then_some(base),
                            )
                            .await
                            .map(Payload::FileDiff),
                        },
                        Command::ListDirectory { path, .. } => {
                            crate::files::list_directory(&root, &path)
                                .await
                                .map(Payload::Directory)
                        }
                        Command::ReadFile { path, .. } => crate::files::read_file(&root, &path)
                            .await
                            .map(Payload::FileContent),
                        Command::ReadFileChunk {
                            path, offset, len, ..
                        } => crate::files::read_chunk(&root, &path, offset, len)
                            .await
                            .map(Payload::FileChunk),
                        Command::WriteFile {
                            path,
                            text,
                            expected,
                            ..
                        } => crate::files::write_file(&workspace, &path, text, expected)
                            .await
                            .map(Payload::FileWritten),
                        _ if !nested.is_empty() => crate::git::nested_changes(&root, &nested)
                            .await
                            .map(Payload::WorkspaceChanges),
                        _ => crate::git::changes(&root, base)
                            .await
                            .map(Payload::WorkspaceChanges),
                    }
                });
            }
            Command::UploadChunk {
                upload,
                path,
                offset,
                data,
            } => {
                let workspace = self.agent.workspace().clone();
                let uploads = self.uploads.clone();
                self.reply_later(id, async move {
                    uploads
                        .chunk(workspace.root(), upload, &path, offset, data)
                        .await
                });
            }
            Command::FinishUpload {
                upload,
                size,
                overwrite,
            } => {
                let workspace = self.agent.workspace().clone();
                let uploads = self.uploads.clone();
                self.reply_later(id, async move {
                    uploads.finish(&workspace, upload, size, overwrite).await
                });
            }
            Command::CancelUpload { upload } => {
                let uploads = self.uploads.clone();
                self.reply_later(id, async move {
                    uploads.cancel(upload).await;
                    Ok(())
                });
            }
            Command::GetSystemPrompt { session } => {
                let session = self.agent.session(&session);
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = match session {
                        Ok(session) => Ok(Payload::SystemPrompt(session.system_prompt().await)),
                        Err(err) => Err(convert::api_error(&err)),
                    };
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::ListModels { .. }
            | Command::GetUsageLimits { .. }
            | Command::StartLogin { .. }
            | Command::CompleteLogin { .. }
            | Command::Logout { .. } => {
                let agent = self.agent.clone();
                let out = self.out.clone();
                let logins = self.logins.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = slow_command(&agent, &out, &logins, command).await;
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::Subscribe {
                session,
                since_seq,
                tail,
            } => match self.agent.session(&session) {
                Ok(handle) => {
                    let subscription = handle.subscribe_tail(since_seq, tail);
                    self.respond(id, Ok(Payload::Session(handle.info()))).await;
                    if let Some(old) = self.subscriptions.remove(&session) {
                        old.abort();
                    }
                    let task = tokio::spawn(forward_events(handle, subscription, self.out.clone()));
                    self.subscriptions.insert(session, task);
                }
                Err(err) => self.respond(id, Err(convert::api_error(&err))).await,
            },
            Command::Unsubscribe { session } => {
                let result = match self.subscriptions.remove(&session) {
                    Some(task) => {
                        task.abort();
                        Ok(Payload::Ok)
                    }
                    None => Err(ApiError::new(
                        ErrorCode::NotFound,
                        format!("not subscribed to {session}"),
                    )),
                };
                self.respond(id, result).await;
            }
            Command::OpenTerminal { cols, rows, owner } => {
                let workspace = self.agent.workspace().clone();
                let config = self.agent.config();
                let shell = config
                    .terminal
                    .shell
                    .clone()
                    .unwrap_or_else(|| interactive_shell(&config.tools.shell.program));
                let env = self.agent.environment().command.vars.clone();
                let result = self
                    .agent
                    .terminals()
                    .open(self.viewer(owner), shell, workspace.root(), env, cols, rows)
                    .map(Payload::Terminal);
                self.respond(id, result).await;
            }
            Command::TerminalInput { terminal, data } => {
                let result = self
                    .agent
                    .terminals()
                    .input(self.connection_id, terminal, data)
                    .await
                    .map(|()| Payload::Ok);
                self.respond(id, result).await;
            }
            Command::ResizeTerminal {
                terminal,
                cols,
                rows,
            } => {
                let result = self
                    .agent
                    .terminals()
                    .resize(self.connection_id, terminal, cols, rows)
                    .map(|()| Payload::Ok);
                self.respond(id, result).await;
            }
            Command::AttachTerminal {
                terminal,
                cols,
                rows,
                owner,
                take_over,
            } => {
                let result = self
                    .agent
                    .terminals()
                    .attach(self.viewer(owner), terminal, cols, rows, take_over)
                    .await
                    .map(|()| Payload::Ok);
                self.respond(id, result).await;
            }
            Command::CloseTerminal { terminal } => {
                let result = self
                    .agent
                    .terminals()
                    .close(self.connection_id, terminal)
                    .map(|()| Payload::Ok);
                self.respond(id, result).await;
            }
            Command::ListTerminals => {
                let terminals = self.agent.terminals().list(self.connection_id);
                self.respond(id, Ok(Payload::Terminals(terminals))).await;
            }
            Command::ListPorts => {
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = crate::ports::listening_ports().await.map(Payload::Ports);
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::OpenTunnel { tunnel, host, port } => {
                let tunnels = self.tunnels.clone();
                self.tasks.push(tokio::spawn(async move {
                    tunnels.open(id, tunnel, host, port).await;
                }));
            }
            Command::TunnelData { tunnel, data } => {
                // The tunnel's writer answers once the bytes are written.
                if let Err(err) = self.tunnels.data(id, tunnel, data) {
                    self.respond(id, Err(err)).await;
                }
            }
            Command::TunnelEnd { tunnel } => {
                if let Err(err) = self.tunnels.end(id, tunnel) {
                    self.respond(id, Err(err)).await;
                }
            }
            Command::TunnelAck { tunnel, bytes } => {
                let result = self.tunnels.ack(tunnel, bytes).map(|()| Payload::Ok);
                self.respond(id, result).await;
            }
            Command::CloseTunnel { tunnel } => {
                let result = self.tunnels.close(tunnel).map(|()| Payload::Ok);
                self.respond(id, result).await;
            }
            Command::CancelLogin { login } => {
                let waiting = self
                    .logins
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&login);
                let found = match waiting {
                    Some(token) => {
                        token.cancel();
                        true
                    }
                    None => self.agent.auth().cancel_login(&login),
                };
                let result = if found {
                    Ok(Payload::Ok)
                } else {
                    Err(ApiError::new(
                        ErrorCode::NotFound,
                        format!("no pending login {login}"),
                    ))
                };
                self.respond(id, result).await;
            }
            command => {
                let result = self.quick_command(command).await;
                self.respond(id, result).await;
            }
        }
    }

    async fn quick_command(&self, command: Command) -> Reply {
        let agent = &self.agent;
        let session = |id: &SessionId| agent.session(id).map_err(|e| convert::api_error(&e));
        let llm = |e: loom_llm::LlmError| convert::llm_error(&e);
        match command {
            Command::GetAgentInfo => Ok(Payload::AgentInfo(self.agent_info())),
            Command::GetMetrics => Ok(Payload::Metrics(agent.metrics_snapshot())),
            Command::GetTelemetryStatus => Ok(Payload::Telemetry(agent.telemetry_status())),
            Command::ListProviders => {
                let runtime = agent.runtime();
                let statuses = agent.auth().status().await.map_err(llm)?;
                Ok(Payload::Providers(
                    statuses
                        .into_iter()
                        .map(|s| {
                            let enabled = runtime.providers.contains(&s.provider);
                            convert::provider_status(s, enabled)
                        })
                        .collect(),
                ))
            }
            Command::SetApiKey { provider, api_key } => {
                agent
                    .auth()
                    .set_api_key(&ProviderId::new(provider), api_key)
                    .await
                    .map_err(llm)?;
                Ok(Payload::Ok)
            }
            Command::SelectAccount { provider, account } => {
                agent
                    .auth()
                    .select_account(&ProviderId::new(provider), &account)
                    .await
                    .map_err(llm)?;
                Ok(Payload::Ok)
            }
            Command::ListSessions => Ok(Payload::Sessions(agent.list_sessions())),
            Command::CreateSession { model } => agent
                .create_session(model.as_deref())
                .map(|s| Payload::Session(s.info()))
                .map_err(|e| convert::api_error(&e)),
            Command::SendPrompt {
                session: id,
                content,
                delivery,
            } => {
                session(&id)?
                    .send_message(content, delivery)
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Ok)
            }
            Command::CancelPending {
                session: id,
                id: pending,
            } => session(&id)?
                .cancel_pending(pending)
                .map(Payload::PendingCancelled)
                .map_err(|e| convert::api_error(&e)),
            Command::Interrupt { session: id } => {
                Ok(Payload::Interrupted(session(&id)?.interrupt()))
            }
            Command::CompactSession { session: id } => {
                session(&id)?
                    .compact()
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Ok)
            }
            Command::ClearSession { session: id } => {
                session(&id)?.clear().map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Ok)
            }
            Command::Rewind { session: id, seq } => session(&id)?
                .rewind(seq)
                .map(Payload::Rewound)
                .map_err(|e| convert::api_error(&e)),
            Command::MarkSessionSeen { session: id } => {
                session(&id)?.mark_seen();
                Ok(Payload::Ok)
            }
            Command::GetHistory {
                session: id,
                before_seq,
                items,
            } => Ok(Payload::History(
                session(&id)?.older_events(before_seq, items),
            )),
            Command::GetNotes => Ok(Payload::Notes(agent.notes_content())),
            Command::WriteNotes { text, expected } => agent
                .write_notes(text, expected)
                .map(Payload::NotesWritten)
                .map_err(|e| convert::api_error(&e)),
            Command::GetTasks { session: id } => Ok(Payload::Tasks(session(&id)?.tasks_content())),
            Command::UpdateTasks {
                session: id,
                changes,
            } => session(&id)?
                .update_tasks(changes)
                .map(Payload::Tasks)
                .map_err(|e| convert::api_error(&e)),
            Command::ResolveApproval {
                session: id,
                call_id,
                decision,
            } => {
                session(&id)?
                    .resolve_approval(&call_id, decision)
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Ok)
            }
            Command::SetModel { session: id, model } => {
                let handle = session(&id)?;
                handle
                    .set_model(&model)
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Session(handle.info()))
            }
            Command::SetReasoning {
                session: id,
                effort,
            } => {
                let handle = session(&id)?;
                handle.set_reasoning(convert::reasoning_from_proto(effort));
                Ok(Payload::Session(handle.info()))
            }
            Command::SetPolicy {
                session: id,
                default,
            } => {
                let handle = session(&id)?;
                handle.set_policy_default(convert::policy_from_proto(default));
                Ok(Payload::Session(handle.info()))
            }
            Command::SetCommentGuard {
                session: id,
                enabled,
            } => {
                let handle = session(&id)?;
                handle.set_comment_guard(enabled);
                Ok(Payload::Session(handle.info()))
            }
            Command::RenameSession { session: id, title } => self
                .agent
                .rename_session(&id, title)
                .map(Payload::Session)
                .map_err(|e| convert::api_error(&e)),
            Command::ArchiveSession {
                session: id,
                archived,
            } => self
                .agent
                .set_archived(&id, archived)
                .map(Payload::Session)
                .map_err(|e| convert::api_error(&e)),
            Command::ListAgents | Command::SpawnAgent { .. } | Command::StopAgent { .. } => {
                Err(ApiError::new(
                    ErrorCode::Unsupported,
                    "this agent cannot manage other agents",
                ))
            }
            Command::GetHostKey
            | Command::AuthorizeClient { .. }
            | Command::ListClients
            | Command::RevokeClient { .. } => Err(ApiError::new(
                ErrorCode::Unsupported,
                "the oracle manages the clients of this agent",
            )),
            Command::SetPushToken { .. } => Err(ApiError::new(
                ErrorCode::Unsupported,
                "the oracle sets the push tokens of this agent",
            )),
            Command::UpdateAgent { .. } => Err(ApiError::new(
                ErrorCode::Unsupported,
                "this agent cannot update itself",
            )),
            Command::ListModels { .. }
            | Command::GetUsageLimits { .. }
            | Command::StartLogin { .. }
            | Command::CompleteLogin { .. }
            | Command::CancelLogin { .. }
            | Command::Logout { .. }
            | Command::Subscribe { .. }
            | Command::Unsubscribe { .. }
            | Command::ReloadConfig
            | Command::OpenTerminal { .. }
            | Command::TerminalInput { .. }
            | Command::ResizeTerminal { .. }
            | Command::CloseTerminal { .. }
            | Command::AttachTerminal { .. }
            | Command::ListTerminals
            | Command::ListPorts
            | Command::OpenTunnel { .. }
            | Command::TunnelData { .. }
            | Command::TunnelAck { .. }
            | Command::TunnelEnd { .. }
            | Command::CloseTunnel { .. }
            | Command::ListRepositories
            | Command::PushRepositories(_)
            | Command::OpenPullRequests { .. }
            | Command::DeleteSession { .. }
            | Command::GetWorkspaceChanges
            | Command::GetFileDiff { .. }
            | Command::ListDirectory { .. }
            | Command::ReadFile { .. }
            | Command::ReadFileChunk { .. }
            | Command::WriteFile { .. }
            | Command::UploadChunk { .. }
            | Command::FinishUpload { .. }
            | Command::CancelUpload { .. }
            | Command::GetSystemPrompt { .. } => Err(ApiError::new(
                ErrorCode::Internal,
                "command routed to the wrong handler",
            )),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.agent.metrics().connection_closed();
        let timeout = self.agent.config().terminal.detached_timeout.as_duration();
        self.agent
            .terminals()
            .detach_connection(self.connection_id, timeout);
        self.tunnels.close_all();
        for (_, task) in self.subscriptions.drain() {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        for (_, token) in self
            .logins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            token.cancel();
        }
    }
}

async fn manage_repositories(repositories: Option<&dyn Repositories>, command: Command) -> Reply {
    let Some(repositories) = repositories else {
        return Err(ApiError::new(
            ErrorCode::Unsupported,
            "this agent does not manage the repositories of a codespace",
        ));
    };
    match command {
        Command::ListRepositories => repositories.list().await.map(Payload::Repositories),
        Command::PushRepositories(request) => {
            repositories.push(request).await.map(Payload::Repositories)
        }
        Command::OpenPullRequests { title, body, draft } => repositories
            .open_pull_requests(title, body, draft)
            .await
            .map(Payload::PullRequests),
        _ => Err(ApiError::new(
            ErrorCode::Internal,
            "command routed to the wrong handler",
        )),
    }
}

async fn slow_command(
    agent: &Agent,
    out: &mpsc::Sender<ServerMessage>,
    logins: &Arc<Mutex<HashMap<String, CancellationToken>>>,
    command: Command,
) -> Reply {
    let llm = |e: loom_llm::LlmError| convert::llm_error(&e);
    match command {
        Command::ListModels { provider } => {
            let id = ProviderId::new(provider);
            let models = agent
                .runtime()
                .providers
                .get(&id)
                .map_err(llm)?
                .list_models()
                .await
                .map_err(llm)?;
            Ok(Payload::Models(
                models
                    .into_iter()
                    .map(|m| ModelInfo {
                        provider: id.to_string(),
                        id: m.id,
                        display_name: m.display_name,
                    })
                    .collect(),
            ))
        }
        Command::GetUsageLimits { provider } => {
            let limits = agent
                .runtime()
                .providers
                .get(&ProviderId::new(provider))
                .map_err(llm)?
                .usage_limits()
                .await
                .map_err(llm)?;
            Ok(Payload::UsageLimits(limits.map(convert::usage_limits)))
        }
        Command::StartLogin { provider, method } => {
            let started = agent
                .auth()
                .start_login(&ProviderId::new(provider), convert::login_method(method))
                .await
                .map_err(llm)?;
            let started = convert::login_started(started);
            if matches!(started.prompt, LoginPrompt::DeviceCode { .. }) {
                let token = CancellationToken::new();
                logins
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(started.login_id.clone(), token.clone());
                let agent = agent.clone();
                let out = out.clone();
                let logins = logins.clone();
                let login = started.login_id.clone();
                tokio::spawn(async move {
                    let result = agent
                        .auth()
                        .wait_login(&login, &token)
                        .await
                        .map(convert::account_info)
                        .map_err(|e| convert::llm_error(&e));
                    logins
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&login);
                    let _ = out
                        .send(ServerMessage::LoginFinished { login, result })
                        .await;
                });
            }
            Ok(Payload::LoginStarted(started))
        }
        Command::CompleteLogin { login, input } => agent
            .auth()
            .complete_login(&login, convert::login_input(input))
            .await
            .map(|a| Payload::Account(convert::account_info(a)))
            .map_err(llm),
        Command::Logout { provider } => {
            agent
                .auth()
                .logout(&ProviderId::new(provider))
                .await
                .map_err(llm)?;
            Ok(Payload::Ok)
        }
        _ => Err(ApiError::new(
            ErrorCode::Internal,
            "command routed to the wrong handler",
        )),
    }
}

async fn forward_notices(
    mut notices: broadcast::Receiver<AgentNotice>,
    out: mpsc::Sender<ServerMessage>,
) {
    loop {
        let notice = match notices.recv().await {
            Ok(AgentNotice::ConfigReloaded { restart_required }) => {
                Notice::ConfigReloaded { restart_required }
            }
            Ok(AgentNotice::ConfigRejected { message }) => Notice::ConfigRejected { message },
            Ok(AgentNotice::ShuttingDown) => Notice::ShuttingDown,
            Ok(AgentNotice::SessionUpdated(info)) => Notice::SessionUpdated(*info),
            Ok(AgentNotice::SessionRemoved(id)) => Notice::SessionRemoved(id),
            Ok(AgentNotice::NotesChanged { version }) => Notice::NotesChanged { version },
            Ok(AgentNotice::TasksChanged { session, tasks }) => {
                Notice::TasksChanged { session, tasks }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if out.send(ServerMessage::Notice(notice)).await.is_err() {
            return;
        }
    }
}

/// Tells the connection which terminals are open, at once and whenever one
/// opens, closes, or moves to another connection.
async fn forward_terminals(
    agent: Agent,
    connection: ConnectionId,
    out: mpsc::Sender<ServerMessage>,
) {
    let mut changes = agent.terminals().subscribe();
    let mut sent = None;
    loop {
        changes.borrow_and_update();
        let terminals = agent.terminals().list(connection);
        if sent.as_ref() != Some(&terminals) {
            let notice = Notice::TerminalsChanged(terminals.clone());
            if out.send(ServerMessage::Notice(notice)).await.is_err() {
                return;
            }
            sent = Some(terminals);
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// The most bytes of events one [`ServerMessage::Events`] holds, unless a
/// single event is larger. Batches keep a client that loads many events, as a
/// whole session, from costing a message each.
const MAX_BATCH_BYTES: usize = 256 * 1024;

/// Sends a subscription's backlog, then its live events as they come, in
/// batches of what is ready, so a burst of events costs a few messages.
async fn forward_events(
    session: Session,
    mut subscription: Subscription,
    out: mpsc::Sender<ServerMessage>,
) {
    let id = session.id().clone();
    let removed = session.removed();
    let mut last = 0;
    loop {
        let backlog = std::mem::take(&mut subscription.backlog);
        let mut window = subscription.window.take();
        if let Some(event) = backlog.last() {
            last = event.seq;
        }
        let mut batches = batches(backlog);
        if batches.is_empty() && window.is_some() {
            // A new window starts the client over, also when it is empty.
            batches.push(Vec::new());
        }
        for events in batches {
            let message = ServerMessage::Events {
                session: id.clone(),
                events,
                window: window.take(),
            };
            if out.send(message).await.is_err() {
                return;
            }
        }
        loop {
            let received = tokio::select! {
                _ = removed.cancelled() => return,
                received = subscription.live.recv() => received,
            };
            let mut events = Vec::new();
            let mut bytes = 0;
            let mut lagged = false;
            let mut closed = false;
            let mut next = Some(received);
            while let Some(received) = next.take() {
                match received {
                    Ok(event) if event.seq <= last => {}
                    Ok(event) => {
                        last = event.seq;
                        bytes += event_size(&event);
                        events.push(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(session = %id, skipped, "subscriber lagged; resyncing");
                        lagged = true;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        closed = true;
                        break;
                    }
                }
                if bytes < MAX_BATCH_BYTES {
                    // Takes what else is ready, without waiting for more.
                    next = match subscription.live.try_recv() {
                        Ok(event) => Some(Ok(event)),
                        Err(broadcast::error::TryRecvError::Empty) => None,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            Some(Err(broadcast::error::RecvError::Lagged(n)))
                        }
                        Err(broadcast::error::TryRecvError::Closed) => {
                            Some(Err(broadcast::error::RecvError::Closed))
                        }
                    };
                }
            }
            if !events.is_empty() {
                let message = ServerMessage::Events {
                    session: id.clone(),
                    events,
                    window: None,
                };
                if out.send(message).await.is_err() {
                    return;
                }
            }
            if closed {
                return;
            }
            if lagged {
                subscription = session.subscribe(Some(last));
                break;
            }
        }
    }
}

/// Splits events into batches of at most [`MAX_BATCH_BYTES`].
fn batches(events: Vec<SequencedEvent>) -> Vec<Vec<SequencedEvent>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut bytes = 0;
    for event in events {
        let size = event_size(&event);
        if !batch.is_empty() && bytes + size > MAX_BATCH_BYTES {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes += size;
        batch.push(event);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn event_size(event: &SequencedEvent) -> usize {
    loom_proto::encode(event).map_or(0, |bytes| bytes.len())
}
