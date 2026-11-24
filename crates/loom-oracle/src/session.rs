use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use loom_oracle_proto::{
    ClientMessage, CodespaceConnections, CodespaceResult, Codespaces, Command, DeviceResult,
    Devices, Empty, ErrorCode, Event, FirstMessageResult, GitCredentialResult, GitCredentials,
    GitProviderResult, GitProviders, HostResult, Hosts, InvalidMessage, ModelAccountResult,
    ModelAccounts, OracleError, PROTOCOL_VERSION, Pairings, PeerInfo, PresetResult, Presets,
    RelayHostResult, RelayHosts, Request, Response, Role, SecretResult, Secrets, ServerHello,
    ServerMessage, Sessions, VolumeResult, Volumes, Warnings, WorkspaceResult, Workspaces, encode,
    parse_client_message,
};
use loom_transport::{FrameReceiver, FrameStream};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::oracle::{NewCodespaceArgs, Oracle, pairing_ttl};
use crate::store::PeerRow;

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const OUT_QUEUE: usize = 256;
const WRITER_DRAIN: Duration = Duration::from_secs(2);

type Later = Pin<Box<dyn Future<Output = Result<Value, OracleError>> + Send>>;

fn ok(value: impl Serialize) -> Result<Value, OracleError> {
    serde_json::to_value(value).map_err(|e| OracleError::new(ErrorCode::Internal, e.to_string()))
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<Event>>,
) -> Result<Event, broadcast::error::RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_call(calls: &mut Option<mpsc::Receiver<ServerMessage>>) -> Option<ServerMessage> {
    match calls {
        Some(calls) => calls.recv().await,
        None => std::future::pending().await,
    }
}

fn forbidden(role: Role, who: &str) -> OracleError {
    OracleError::new(
        ErrorCode::Forbidden,
        format!("a {role} cannot do this; only {who} can"),
    )
}

fn shown_to(role: Role, event: &Event) -> bool {
    match role {
        Role::Device => !matches!(event, Event::DeviceKeysChanged | Event::GitChanged),
        Role::Codespace => matches!(
            event,
            Event::DeviceKeysChanged
                | Event::GitChanged
                | Event::ConfigChanged { .. }
                | Event::EventsLost
        ),
        _ => false,
    }
}

fn response(id: u64, result: Result<Value, OracleError>) -> ServerMessage {
    ServerMessage::Response(match result {
        Ok(result) => Response {
            id,
            result: Some(result),
            error: None,
        },
        Err(error) => Response::error(id, error),
    })
}

#[allow(clippy::large_enum_variant)]
enum Step {
    Stop,
    Frame(Bytes),
    Event(Result<Event, broadcast::error::RecvError>),
    Call(ServerMessage),
}

pub struct Session {
    oracle: Arc<Oracle>,
    peer: PeerRow,
    kill: CancellationToken,
    events: Option<broadcast::Receiver<Event>>,
    calls: Option<mpsc::Receiver<ServerMessage>>,
}

impl Session {
    pub fn new(
        oracle: Arc<Oracle>,
        peer: PeerRow,
        kill: CancellationToken,
        calls: Option<mpsc::Receiver<ServerMessage>>,
    ) -> Self {
        Self {
            oracle,
            peer,
            kill,
            events: None,
            calls,
        }
    }

    pub async fn run(mut self, stream: FrameStream) {
        let (mut sender, mut receiver, _) = stream.split();
        let (out, mut queued) = mpsc::channel::<Vec<u8>>(OUT_QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(bytes) = queued.recv().await {
                if sender.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
            let _ = sender.close().await;
        });
        if self.handshake(&mut receiver, &out).await {
            self.serve(&mut receiver, &out).await;
        }
        drop(out);
        let _ = tokio::time::timeout(WRITER_DRAIN, writer).await;
    }

    async fn send(&self, out: &mpsc::Sender<Vec<u8>>, message: &ServerMessage) -> bool {
        let Ok(bytes) = encode(message) else {
            return false;
        };
        tokio::select! {
            biased;
            sent = out.send(bytes) => sent.is_ok(),
            _ = self.kill.cancelled() => false,
        }
    }

    async fn handshake(&self, receiver: &mut FrameReceiver, out: &mpsc::Sender<Vec<u8>>) -> bool {
        let frame = tokio::select! {
            _ = self.kill.cancelled() => return false,
            frame = tokio::time::timeout(HELLO_TIMEOUT, receiver.recv()) => frame,
        };
        let Ok(Ok(Some(bytes))) = frame else {
            return false;
        };
        match parse_client_message(&bytes) {
            Ok(ClientMessage::Hello(hello)) => {
                tracing::debug!(
                    peer = %self.peer.label,
                    client = %hello.client,
                    version = hello.version,
                    "control client said hello"
                );
                let reply = ServerMessage::Hello(ServerHello {
                    version: PROTOCOL_VERSION,
                    oracle: self.oracle.info(),
                    peer: PeerInfo {
                        id: self.peer.id,
                        role: self.peer.role,
                        label: self.peer.label.clone(),
                    },
                });
                self.send(out, &reply).await
            }
            _ => {
                let error =
                    OracleError::new(ErrorCode::BadRequest, "the first message must be hello");
                self.send(out, &ServerMessage::Rejected(error)).await;
                false
            }
        }
    }

    async fn serve(&mut self, receiver: &mut FrameReceiver, out: &mpsc::Sender<Vec<u8>>) {
        loop {
            let step = tokio::select! {
                _ = self.kill.cancelled() => Step::Stop,
                frame = receiver.recv() => match frame {
                    Ok(Some(bytes)) => Step::Frame(bytes),
                    _ => Step::Stop,
                },
                event = next_event(&mut self.events) => Step::Event(event),
                call = next_call(&mut self.calls) => match call {
                    Some(call) => Step::Call(call),
                    None => Step::Stop,
                },
            };
            let message = match step {
                Step::Stop => break,
                Step::Call(call) => call,
                Step::Event(Ok(event)) if !shown_to(self.peer.role, &event) => continue,
                Step::Event(Ok(event)) => ServerMessage::Event {
                    event: Box::new(event),
                },
                Step::Event(Err(broadcast::error::RecvError::Lagged(_))) => ServerMessage::Event {
                    event: Box::new(Event::EventsLost),
                },
                Step::Event(Err(broadcast::error::RecvError::Closed)) => {
                    self.events = None;
                    continue;
                }
                Step::Frame(bytes) => match parse_client_message(&bytes) {
                    Ok(ClientMessage::Request(Request { id, command })) => {
                        match self.later(command) {
                            Ok(later) => {
                                let out = out.clone();
                                let kill = self.kill.clone();
                                tokio::spawn(async move {
                                    let result = tokio::select! {
                                        result = later => result,
                                        _ = kill.cancelled() => return,
                                    };
                                    if let Ok(bytes) = encode(&response(id, result)) {
                                        let _ = out.send(bytes).await;
                                    }
                                });
                                continue;
                            }
                            Err(command) => response(id, self.handle(*command)),
                        }
                    }
                    Ok(ClientMessage::Hello(_)) => continue,
                    Ok(ClientMessage::Reply(reply)) => {
                        if self.peer.role == Role::Host {
                            self.oracle.reply(self.peer.id, reply);
                        }
                        continue;
                    }
                    Err(InvalidMessage {
                        id: Some(id),
                        error,
                    }) => ServerMessage::Response(Response::error(id, error)),
                    Err(InvalidMessage { id: None, error }) => {
                        tracing::debug!(peer = %self.peer.label, %error, "ignored a malformed message");
                        continue;
                    }
                },
            };
            if !self.send(out, &message).await {
                break;
            }
        }
    }

    fn handle(&mut self, command: Command) -> Result<Value, OracleError> {
        if command == Command::OracleInfo {
            return ok(self.oracle.info());
        }
        match self.peer.role {
            Role::Device => self.handle_device(command),
            Role::Host => self.handle_host(command),
            Role::Codespace => self.handle_codespace(command),
            Role::Unknown => Err(forbidden(self.peer.role, "known peers")),
        }
    }

    fn later(&self, command: Command) -> Result<Later, Box<Command>> {
        let oracle = self.oracle.clone();
        let me = Some(self.peer.id);
        if self.peer.role == Role::Codespace {
            let peer = self.peer.id;
            return match command {
                Command::ModelCredentials => Ok(Box::pin(async move {
                    ok(oracle.model_credentials(peer).await?)
                })),
                Command::GitCredential { url } => {
                    Ok(Box::pin(
                        async move { ok(oracle.git_token(peer, &url).await?) },
                    ))
                }
                Command::CodespaceApiRequest(request) => Ok(Box::pin(async move {
                    oracle.codespace_api_request(peer, &request).await
                })),
                other => Err(Box::new(other)),
            };
        }
        if self.peer.role == Role::Host {
            let peer = self.peer.id;
            return match command {
                Command::GitProxyAccess(request) => Ok(Box::pin(async move {
                    ok(oracle.git_proxy_access(peer, &request).await?)
                })),
                other => Err(Box::new(other)),
            };
        }
        if self.peer.role != Role::Device {
            return Err(Box::new(command));
        }
        Ok(match command {
            Command::CreateWorkspace {
                name,
                host,
                repositories,
                volumes,
            } => Box::pin(async move {
                ok(WorkspaceResult {
                    workspace: oracle
                        .create_workspace(&name, &host, &repositories, &volumes, me)
                        .await?,
                })
            }),
            Command::UpdateWorkspace {
                name,
                host,
                repositories,
                volumes,
            } => Box::pin(async move {
                ok(WorkspaceResult {
                    workspace: oracle
                        .update_workspace(
                            &name,
                            host.as_deref(),
                            repositories.as_deref(),
                            volumes.as_deref(),
                            me,
                        )
                        .await?,
                })
            }),
            Command::DeleteWorkspace { name } => Box::pin(async move {
                oracle.delete_workspace(&name, me).await?;
                ok(Empty {})
            }),
            Command::CreateCodespace {
                workspace,
                preset,
                cpus,
                memory_mib,
                base_branch,
                base_branches,
                client_version,
                first_message,
            } => Box::pin(async move {
                let args = NewCodespaceArgs {
                    workspace,
                    preset,
                    cpus,
                    memory_mib,
                    base_branch,
                    base_branches,
                    client_version,
                    first_message,
                };
                let (codespace, warnings) = oracle.create_codespace(args, me).await?;
                ok(CodespaceResult {
                    codespace,
                    warnings,
                })
            }),
            Command::SetGitCredential(input) => Box::pin(async move {
                ok(GitCredentialResult {
                    credential: oracle.set_git_credential(input, me).await?,
                })
            }),
            Command::CheckGitCredential { name } => Box::pin(async move {
                ok(GitCredentialResult {
                    credential: oracle.check_git_credential(&name).await?,
                })
            }),
            Command::SearchRepositories { provider, query } => {
                Box::pin(async move { ok(oracle.search_repositories(&provider, &query).await?) })
            }
            Command::ListBranches {
                provider,
                owner,
                repository,
            } => {
                Box::pin(
                    async move { ok(oracle.list_branches(&provider, &owner, &repository).await?) },
                )
            }
            Command::DeleteVolume { name } => Box::pin(async move {
                oracle.delete_volume(&name, me).await?;
                ok(Empty {})
            }),
            Command::CheckHost { name, docker } => Box::pin(async move {
                ok(HostResult {
                    host: oracle.run_host_check(&name, docker, me).await?,
                })
            }),
            Command::HostVolumes { name } => {
                Box::pin(async move { ok(oracle.host_volumes(&name).await?) })
            }
            Command::StartModelLogin {
                account,
                provider,
                method,
            } => Box::pin(async move {
                ok(oracle
                    .start_model_login(&account, &provider, method, me)
                    .await?)
            }),
            Command::CompleteModelLogin { login, input } => Box::pin(async move {
                ok(ModelAccountResult {
                    account: oracle.complete_model_login(&login, input, me).await?,
                })
            }),
            Command::SetModelApiKey {
                account,
                provider,
                api_key,
            } => Box::pin(async move {
                ok(ModelAccountResult {
                    account: oracle
                        .set_model_api_key(&account, &provider, &api_key, me)
                        .await?,
                })
            }),
            Command::ListModels { preset } => {
                Box::pin(async move { ok(oracle.list_models(&preset).await?) })
            }
            other => return Err(Box::new(other)),
        })
    }

    fn subscribe(&mut self, subscribe: bool) -> Result<Value, OracleError> {
        if !subscribe {
            self.events = None;
        } else if self.events.is_none() {
            self.events = Some(self.oracle.subscribe());
        }
        ok(Empty {})
    }

    fn handle_host(&mut self, command: Command) -> Result<Value, OracleError> {
        let oracle = &self.oracle;
        let me = self.peer.id;
        match command {
            Command::HostReport { report } => {
                oracle.host_report(me, &report)?;
                ok(Empty {})
            }
            Command::HostInstances { instances } => {
                oracle.host_instances(me, instances)?;
                ok(Empty {})
            }
            Command::InstanceProgress { codespace, step } => {
                oracle.instance_progress(me, &codespace, &step)?;
                ok(Empty {})
            }
            _ => Err(forbidden(self.peer.role, "devices")),
        }
    }

    fn handle_codespace(&mut self, command: Command) -> Result<Value, OracleError> {
        let me = self.peer.id;
        match command {
            Command::Subscribe => self.subscribe(true),
            Command::Unsubscribe => self.subscribe(false),
            Command::CodespaceBundle => ok(self.oracle.bundle(me)?),
            Command::CodespaceAgent { address } => {
                self.oracle.codespace_agent(me, &address)?;
                ok(Empty {})
            }
            Command::CodespaceProgress { step } => {
                self.oracle.codespace_progress(me, &step)?;
                ok(Empty {})
            }
            Command::CodespaceReady => {
                self.oracle.codespace_ready(me)?;
                ok(Empty {})
            }
            Command::CodespaceFailed { error } => {
                self.oracle.codespace_failed(me, &error)?;
                ok(Empty {})
            }
            Command::CodespaceNotes { text } => {
                self.oracle.codespace_notes(me, &text)?;
                ok(Empty {})
            }
            Command::SessionSummaries {
                sessions,
                removed,
                full,
            } => {
                self.oracle
                    .session_summaries(me, &sessions, &removed, full)?;
                ok(Empty {})
            }
            Command::TakeFirstMessage => ok(FirstMessageResult {
                message: self.oracle.take_first_message(me)?,
            }),
            Command::CodespaceActivity { active, metrics } => {
                self.oracle.codespace_activity(me, active, metrics)?;
                ok(Empty {})
            }
            _ => Err(forbidden(self.peer.role, "devices")),
        }
    }

    fn handle_device(&mut self, command: Command) -> Result<Value, OracleError> {
        let oracle = self.oracle.clone();
        let me = Some(self.peer.id);
        match command {
            Command::OracleInfo => ok(oracle.info()),
            Command::Subscribe => self.subscribe(true),
            Command::Unsubscribe => self.subscribe(false),
            Command::ListDevices => ok(Devices {
                devices: oracle.devices()?,
            }),
            Command::RenameDevice { id, label } => ok(DeviceResult {
                device: oracle.rename_device(id, &label, me)?,
            }),
            Command::RevokeDevice { id } => {
                oracle.revoke_device(id, me)?;
                ok(Empty {})
            }
            Command::SetPushToken { token } => {
                oracle.set_push_token(self.peer.id, token.as_deref())?;
                ok(Empty {})
            }
            Command::CreateDevicePairing { ttl_secs } => {
                ok(oracle.create_pairing(Role::Device, pairing_ttl(ttl_secs)?, me)?)
            }
            Command::ListPairings => ok(Pairings {
                pairings: oracle.pairings()?,
            }),
            Command::CancelPairing { id } => {
                oracle.cancel_pairing(&id, me)?;
                ok(Empty {})
            }
            Command::ListRelayHosts => ok(RelayHosts {
                relay_hosts: oracle.relay_hosts()?,
            }),
            Command::AddRelayHost { host_id, label } => ok(RelayHostResult {
                relay_host: oracle.add_relay_host(&host_id, &label, me)?,
            }),
            Command::RemoveRelayHost { host_id } => {
                oracle.remove_relay_host(&host_id, me)?;
                ok(Empty {})
            }
            Command::CreateHostPairing { ttl_secs } => {
                ok(oracle.create_pairing(Role::Host, pairing_ttl(ttl_secs)?, me)?)
            }
            Command::ListHosts => ok(Hosts {
                hosts: oracle.hosts()?,
            }),
            Command::RemoveHost { name } => {
                oracle.remove_host(&name, me)?;
                ok(Empty {})
            }
            Command::SetHostEnvironment { name, environment } => ok(HostResult {
                host: oracle.set_host_environment(&name, &environment, me)?,
            }),
            Command::SetHostSsh { name, destination } => ok(HostResult {
                host: oracle.set_host_ssh(&name, destination.as_deref(), me)?,
            }),
            Command::ListWorkspaces => ok(Workspaces {
                workspaces: oracle.workspaces()?,
            }),
            Command::ListCodespaces => ok(Codespaces {
                codespaces: oracle.codespaces()?,
            }),
            Command::CloseCodespace { id } => ok(CodespaceResult {
                codespace: oracle.close_codespace(&id, me)?,
                warnings: Vec::new(),
            }),
            Command::StopCodespace { id } => ok(CodespaceResult {
                codespace: oracle.stop_codespace(&id, me)?,
                warnings: Vec::new(),
            }),
            Command::ResumeCodespace { id } => ok(CodespaceResult {
                codespace: oracle.resume_codespace(&id, me)?,
                warnings: Vec::new(),
            }),
            Command::CodespaceConnections { id, types } => ok(CodespaceConnections {
                connections: oracle.codespace_connections(&id, &types)?,
            }),
            Command::SetHostLimits { name, limits } => ok(HostResult {
                host: oracle.set_host_limits(&name, &limits, me)?,
            }),
            Command::ListAudit { limit, before } => ok(oracle.audit_entries(limit, before)?),
            Command::CodespaceWarnings { workspace, preset } => ok(Warnings {
                warnings: oracle.codespace_warnings(&workspace, preset.as_deref())?,
            }),
            Command::ListGitProviders => ok(GitProviders {
                providers: oracle.git_providers()?,
            }),
            Command::SetGitProvider(input) => ok(GitProviderResult {
                provider: oracle.set_git_provider(&input, me)?,
            }),
            Command::DeleteGitProvider { name } => {
                oracle.delete_git_provider(&name, me)?;
                ok(Empty {})
            }
            Command::ListGitCredentials => ok(GitCredentials {
                credentials: oracle.git_credentials()?,
            }),
            Command::DeleteGitCredential { name } => {
                oracle.delete_git_credential(&name, me)?;
                ok(Empty {})
            }
            Command::ListPresets => ok(Presets {
                presets: oracle.presets()?,
            }),
            Command::SetPreset {
                name,
                yaml,
                agents_md,
                files,
            } => ok(PresetResult {
                preset: oracle.set_preset(
                    &name,
                    &yaml,
                    agents_md.as_deref(),
                    files.as_deref(),
                    me,
                )?,
            }),
            Command::GetPreset { name } => ok(oracle.preset_detail(&name)?),
            Command::DeletePreset { name } => {
                oracle.delete_preset(&name, me)?;
                ok(Empty {})
            }
            Command::ListModelAccounts => ok(ModelAccounts {
                accounts: oracle.model_accounts()?,
            }),
            Command::CancelModelLogin { login } => {
                oracle.cancel_model_login(&login)?;
                ok(Empty {})
            }
            Command::DeleteModelAccount { name } => {
                oracle.delete_model_account(&name, me)?;
                ok(Empty {})
            }
            Command::ListSecrets => ok(Secrets {
                secrets: oracle.secrets()?,
            }),
            Command::SetSecret { name, value } => ok(SecretResult {
                secret: oracle.set_secret(&name, &value, me)?,
            }),
            Command::DeleteSecret { name } => {
                oracle.delete_secret(&name, me)?;
                ok(Empty {})
            }
            Command::GetWorkspaceNotes { workspace } => ok(oracle.workspace_notes(&workspace)?),
            Command::SetWorkspaceNotes { workspace, text } => {
                ok(oracle.set_workspace_notes(&workspace, &text, me)?)
            }
            Command::ListSessions { codespace } => ok(Sessions {
                sessions: oracle.sessions(codespace.as_deref())?,
            }),
            Command::ListVolumes => ok(Volumes {
                volumes: oracle.volumes()?,
            }),
            Command::CreateVolume(input) => ok(VolumeResult {
                volume: oracle.create_volume(&input, me)?,
            }),
            Command::HostReport { .. }
            | Command::HostInstances { .. }
            | Command::InstanceProgress { .. }
            | Command::GitProxyAccess(_) => Err(forbidden(self.peer.role, "hosts")),
            _ => Err(forbidden(self.peer.role, "codespaces")),
        }
    }
}
