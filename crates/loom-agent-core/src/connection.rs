use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use loom_llm::ProviderId;
use loom_proto::{
    AgentInfo, ApiError, ClientHello, ClientMessage, Command, ErrorCode, LoginPrompt, ModelInfo,
    Notice, PROTOCOL_VERSION, Payload, RequestId, ServerHello, ServerMessage, SessionId,
};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentNotice};
use crate::convert;
use crate::events::Subscription;
use crate::host_keys::HostKeys;
use crate::session::Session;
use crate::supervisor::Supervisor;
use crate::terminals::{ConnectionId, interactive_shell};

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
    supervisor: Option<Arc<dyn Supervisor>>,
    host_keys: Option<Arc<dyn HostKeys>>,
    connection_id: ConnectionId,
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
            out,
            greeted: false,
            subscriptions: HashMap::new(),
            logins: Arc::default(),
            tasks: Vec::new(),
            supervisor: None,
            host_keys: None,
            connection_id,
        }
    }

    pub fn with_host_keys(mut self, host_keys: Option<Arc<dyn HostKeys>>) -> Self {
        self.host_keys = host_keys;
        self
    }

    pub fn with_supervisor(mut self, supervisor: Option<Arc<dyn Supervisor>>) -> Self {
        self.supervisor = supervisor;
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
        }
        Flow::Continue
    }

    async fn respond(&self, id: RequestId, result: Reply) {
        self.send(ServerMessage::Response { id, result }).await;
    }

    async fn request(&mut self, id: RequestId, command: Command) {
        match command {
            Command::ListAgents | Command::SpawnAgent { .. } | Command::StopAgent { .. } => {
                let supervisor = self.supervisor.clone();
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = supervise(supervisor.as_deref(), command).await;
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::GetHostKey
            | Command::AuthorizeClient { .. }
            | Command::ListClients
            | Command::RevokeClient { .. } => {
                let host_keys = self.host_keys.clone();
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = manage_keys(host_keys.as_deref(), command).await;
                    let _ = out.send(ServerMessage::Response { id, result }).await;
                }));
            }
            Command::GetWorkspaceChanges
            | Command::GetFileDiff { .. }
            | Command::ListDirectory { .. }
            | Command::ReadFile { .. } => {
                let root = self.agent.workspace_root().to_path_buf();
                let out = self.out.clone();
                self.tasks.push(tokio::spawn(async move {
                    let result = match command {
                        Command::GetFileDiff { path, old_path } => {
                            crate::git::file_diff(&root, &path, old_path.as_deref())
                                .await
                                .map(Payload::FileDiff)
                        }
                        Command::ListDirectory { path } => {
                            crate::files::list_directory(&root, &path)
                                .await
                                .map(Payload::Directory)
                        }
                        Command::ReadFile { path } => crate::files::read_file(&root, &path)
                            .await
                            .map(Payload::FileContent),
                        _ => crate::git::changes(&root)
                            .await
                            .map(Payload::WorkspaceChanges),
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
            Command::Subscribe { session, since_seq } => match self.agent.session(&session) {
                Ok(handle) => {
                    let subscription = handle.subscribe(since_seq);
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
            Command::OpenTerminal { cols, rows } => {
                let config = self.agent.config();
                let shell = config
                    .terminal
                    .shell
                    .clone()
                    .unwrap_or_else(|| interactive_shell(&config.tools.shell.program));
                let result = self
                    .agent
                    .terminals()
                    .open(
                        self.connection_id,
                        shell,
                        self.agent.workspace_root(),
                        cols,
                        rows,
                        self.out.clone(),
                    )
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
            } => {
                let result = self
                    .agent
                    .terminals()
                    .attach(self.connection_id, terminal, cols, rows, self.out.clone())
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
            } => {
                session(&id)?
                    .send_prompt(content)
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Ok)
            }
            Command::Interrupt { session: id } => {
                Ok(Payload::Interrupted(session(&id)?.interrupt()))
            }
            Command::CompactSession { session: id } => {
                session(&id)?
                    .compact()
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Ok)
            }
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
                    .set_model(model.as_deref())
                    .map_err(|e| convert::api_error(&e))?;
                Ok(Payload::Session(handle.info()))
            }
            Command::SetReasoning {
                session: id,
                effort,
            } => {
                let handle = session(&id)?;
                handle.set_reasoning(effort.map(convert::reasoning_from_proto));
                Ok(Payload::Session(handle.info()))
            }
            Command::SetPolicy {
                session: id,
                default,
            } => {
                let handle = session(&id)?;
                handle.set_policy_default(default.map(convert::policy_from_proto));
                Ok(Payload::Session(handle.info()))
            }
            Command::DeleteSession { session: id } => self
                .agent
                .delete_session(&id)
                .map(|()| Payload::Ok)
                .map_err(|e| convert::api_error(&e)),
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
            Command::ListModels { .. }
            | Command::GetUsageLimits { .. }
            | Command::StartLogin { .. }
            | Command::CompleteLogin { .. }
            | Command::CancelLogin { .. }
            | Command::Logout { .. }
            | Command::Subscribe { .. }
            | Command::Unsubscribe { .. }
            | Command::ListAgents
            | Command::SpawnAgent { .. }
            | Command::StopAgent { .. }
            | Command::OpenTerminal { .. }
            | Command::TerminalInput { .. }
            | Command::ResizeTerminal { .. }
            | Command::CloseTerminal { .. }
            | Command::AttachTerminal { .. }
            | Command::GetHostKey
            | Command::AuthorizeClient { .. }
            | Command::ListClients
            | Command::RevokeClient { .. }
            | Command::GetWorkspaceChanges
            | Command::GetFileDiff { .. }
            | Command::ListDirectory { .. }
            | Command::ReadFile { .. } => Err(ApiError::new(
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

async fn manage_keys(host_keys: Option<&dyn HostKeys>, command: Command) -> Reply {
    let Some(host_keys) = host_keys else {
        return Err(ApiError::new(
            ErrorCode::Unsupported,
            "this agent does not manage relay clients",
        ));
    };
    match command {
        Command::GetHostKey => host_keys.host_key().await.map(Payload::HostKey),
        Command::AuthorizeClient { pubkey, label } => host_keys
            .authorize(pubkey, label)
            .await
            .map(Payload::HostKey),
        Command::ListClients => host_keys.list().await.map(Payload::Clients),
        Command::RevokeClient { label } => host_keys.revoke(label).await.map(|()| Payload::Ok),
        _ => Err(ApiError::new(
            ErrorCode::Internal,
            "command routed to the wrong handler",
        )),
    }
}

async fn supervise(supervisor: Option<&dyn Supervisor>, command: Command) -> Reply {
    let Some(supervisor) = supervisor else {
        return Err(ApiError::new(
            ErrorCode::Unsupported,
            "this agent cannot manage other agents",
        ));
    };
    match command {
        Command::ListAgents => supervisor.list().await.map(Payload::Agents),
        Command::SpawnAgent { workspace } => {
            supervisor.spawn(workspace).await.map(Payload::AgentInfo)
        }
        Command::StopAgent { instance } => supervisor.stop(&instance).await.map(|()| Payload::Ok),
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
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if out.send(ServerMessage::Notice(notice)).await.is_err() {
            return;
        }
    }
}

async fn forward_events(
    session: Session,
    mut subscription: Subscription,
    out: mpsc::Sender<ServerMessage>,
) {
    let id = session.id().clone();
    let removed = session.removed();
    let mut last = 0;
    loop {
        for event in std::mem::take(&mut subscription.backlog) {
            last = event.seq;
            let message = ServerMessage::Event {
                session: id.clone(),
                event,
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
            match received {
                Ok(event) if event.seq <= last => {}
                Ok(event) => {
                    last = event.seq;
                    let message = ServerMessage::Event {
                        session: id.clone(),
                        event,
                    };
                    if out.send(message).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(session = %id, skipped, "subscriber lagged; resyncing");
                    subscription = session.subscribe(Some(last));
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}
