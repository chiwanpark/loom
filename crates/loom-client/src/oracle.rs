use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use loom_crypto::{ClientIdentity, PairingUri, decode_key, encode_key};
use loom_oracle_proto::{
    AgentAddress, Branches, Bundle, ClientHello, ClientMessage, Codespace, CodespaceResult,
    Codespaces, Command, Device, DeviceResult, Devices, Empty, Environment, Event, GitCredential,
    GitCredentialInput, GitCredentialResult, GitCredentials, GitProvider, GitProviderInput,
    GitProviderResult, GitProviders, GitTokenResult, Host, HostCall, HostReport, HostResult, Hosts,
    InstanceReport, ORACLE_INSTANCE, OracleError, OracleInfo, PROTOCOL_VERSION, Pairing,
    PairingLink, Pairings, Preset, PresetResult, Presets, RelayHost, RelayHostResult, RelayHosts,
    Reply, Repository, RepositorySearch, Request, ServerHello, ServerMessage, Workspace,
    WorkspaceResult, Workspaces, encode, parse_server_message,
};
use loom_proto::InstanceId;
use loom_proto::relay::{AgentId, HostId};
use loom_transport::{AccessToken, FrameReceiver, FrameSender, FrameStream, RelayEndpoint};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::error::{ClientError, Result};

const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_QUEUE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleEndpoint {
    pub url: String,
    pub host: HostId,
    pub host_key: [u8; 32],
    pub access: Option<AccessToken>,
}

impl OracleEndpoint {
    pub fn from_parts(
        url: &str,
        host_id: &str,
        host_key: &str,
        access: Option<AccessToken>,
    ) -> Result<Self> {
        let host = host_id
            .parse()
            .map_err(|_| ClientError::Protocol(format!("invalid host ID {host_id:?}")))?;
        let host_key =
            decode_key(host_key).ok_or_else(|| ClientError::Protocol("invalid host key".into()))?;
        Ok(Self {
            url: url.to_owned(),
            host,
            host_key,
            access,
        })
    }

    pub fn host_key_text(&self) -> String {
        encode_key(&self.host_key)
    }

    fn endpoint(&self) -> RelayEndpoint {
        RelayEndpoint {
            url: self.url.clone(),
            access: self.access.clone(),
        }
    }

    pub fn agent(&self) -> AgentId {
        AgentId {
            host: self.host,
            instance: InstanceId(ORACLE_INSTANCE.into()),
        }
    }

    pub async fn connect_stream(&self, identity: &ClientIdentity) -> Result<FrameStream> {
        Ok(
            loom_transport::connect_relay(&self.endpoint(), self.agent(), identity, &self.host_key)
                .await?,
        )
    }
}

pub async fn pair_oracle(
    link: &str,
    identity: &ClientIdentity,
    label: &str,
    access: Option<AccessToken>,
) -> Result<OracleEndpoint> {
    let uri: PairingUri = link.trim().parse()?;
    if uri.instance.0 != ORACLE_INSTANCE {
        return Err(ClientError::Protocol(
            "this link pairs with an agent, not with an oracle".into(),
        ));
    }
    let endpoint = OracleEndpoint {
        url: uri.relay_url.clone(),
        host: uri.host,
        host_key: uri.host_static,
        access,
    };
    loom_transport::pair_relay(&endpoint.endpoint(), &uri, identity, label).await?;
    Ok(endpoint)
}

type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value>>>>>>;

#[derive(Clone)]
pub struct OracleClient {
    hello: Arc<ServerHello>,
    writer: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

pub struct OracleEvents {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl OracleEvents {
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

pub struct OracleCalls {
    rx: mpsc::UnboundedReceiver<(u64, HostCall)>,
}

impl OracleCalls {
    pub async fn recv(&mut self) -> Option<(u64, HostCall)> {
        self.rx.recv().await
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewCodespace {
    pub workspace: String,
    pub preset: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub base_branch: Option<String>,
    pub base_branches: BTreeMap<String, String>,
    pub client_version: Option<String>,
}

fn done(result: Result<Empty>) -> Result<()> {
    result.map(|_| ())
}

fn protocol(err: serde_json::Error) -> ClientError {
    ClientError::Protocol(err.to_string())
}

async fn send_message(sender: &mut FrameSender, message: &ClientMessage) -> Result<()> {
    sender
        .send(Bytes::from(encode(message).map_err(protocol)?))
        .await?;
    Ok(())
}

async fn recv_message(receiver: &mut FrameReceiver) -> Result<Option<ServerMessage>> {
    loop {
        let Some(frame) = receiver.recv().await? else {
            return Ok(None);
        };
        if let Some(message) = parse_server_message(&frame).map_err(protocol)? {
            return Ok(Some(message));
        }
    }
}

impl OracleClient {
    pub async fn connect(
        endpoint: &OracleEndpoint,
        identity: &ClientIdentity,
        client_name: &str,
    ) -> Result<(Self, OracleEvents)> {
        let stream = endpoint.connect_stream(identity).await?;
        Self::over(stream, client_name).await
    }

    pub async fn over(stream: FrameStream, client_name: &str) -> Result<(Self, OracleEvents)> {
        let (client, events, _) = Self::open(stream, client_name, false).await?;
        Ok((client, events))
    }

    pub async fn connect_with_calls(
        endpoint: &OracleEndpoint,
        identity: &ClientIdentity,
        client_name: &str,
    ) -> Result<(Self, OracleEvents, OracleCalls)> {
        let stream = endpoint.connect_stream(identity).await?;
        Self::over_with_calls(stream, client_name).await
    }

    pub async fn over_with_calls(
        stream: FrameStream,
        client_name: &str,
    ) -> Result<(Self, OracleEvents, OracleCalls)> {
        let (client, events, calls) = Self::open(stream, client_name, true).await?;
        let calls = calls.ok_or(ClientError::Closed)?;
        Ok((client, events, calls))
    }

    async fn open(
        stream: FrameStream,
        client_name: &str,
        with_calls: bool,
    ) -> Result<(Self, OracleEvents, Option<OracleCalls>)> {
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
            loop {
                match recv_message(&mut receiver).await? {
                    Some(ServerMessage::Hello(hello)) => return Ok(hello),
                    Some(ServerMessage::Rejected(err)) => return Err(ClientError::Oracle(err)),
                    Some(_) => continue,
                    None => return Err(ClientError::Closed),
                }
            }
        };
        let hello = tokio::time::timeout(HELLO_TIMEOUT, handshake)
            .await
            .map_err(|_| ClientError::Protocol("the oracle did not say hello in time".into()))??;

        let (writer, mut queued) = mpsc::channel::<Vec<u8>>(WRITE_QUEUE);
        tokio::spawn(async move {
            while let Some(bytes) = queued.recv().await {
                if sender.send(Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
            let _ = sender.close().await;
        });
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events, rx) = mpsc::unbounded_channel();
        let (calls, calls_rx) = if with_calls {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(OracleCalls { rx }))
        } else {
            (None, None)
        };
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            loop {
                match recv_message(&mut receiver).await {
                    Ok(Some(ServerMessage::Response(response))) => {
                        let waiter = reader_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_mut()
                            .and_then(|pending| pending.remove(&response.id));
                        if let Some(waiter) = waiter {
                            let result = match response.error {
                                Some(err) => Err(ClientError::Oracle(err)),
                                None => Ok(response
                                    .result
                                    .unwrap_or_else(|| Value::Object(Default::default()))),
                            };
                            let _ = waiter.send(result);
                        }
                    }
                    Ok(Some(ServerMessage::Event { event })) => {
                        let _ = events.send(*event);
                    }
                    Ok(Some(ServerMessage::Call { id, call })) => {
                        if let Some(calls) = &calls {
                            let _ = calls.send((id, call));
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(error = %err, "the oracle connection failed");
                        break;
                    }
                }
            }
            reader_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        });
        Ok((
            Self {
                hello: Arc::new(hello),
                writer,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            OracleEvents { rx },
            calls_rx,
        ))
    }

    pub fn hello(&self) -> &ServerHello {
        &self.hello
    }

    pub fn is_closed(&self) -> bool {
        self.writer.is_closed()
            || self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
    }

    pub async fn reply(
        &self,
        id: u64,
        result: std::result::Result<Value, OracleError>,
    ) -> Result<()> {
        let (result, error) = match result {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        let bytes = encode(&ClientMessage::Reply(Reply { id, result, error })).map_err(protocol)?;
        self.writer
            .send(bytes)
            .await
            .map_err(|_| ClientError::Closed)
    }

    pub async fn call<T: DeserializeOwned>(&self, command: Command) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let Some(pending) = pending.as_mut() else {
                return Err(ClientError::Closed);
            };
            pending.insert(id, tx);
        }
        let bytes = encode(&ClientMessage::Request(Request { id, command })).map_err(protocol)?;
        if self.writer.send(bytes).await.is_err() {
            return Err(ClientError::Closed);
        }
        let value = rx.await.map_err(|_| ClientError::Closed)??;
        serde_json::from_value(value).map_err(protocol)
    }

    pub async fn info(&self) -> Result<OracleInfo> {
        self.call(Command::OracleInfo).await
    }

    pub async fn subscribe(&self) -> Result<()> {
        self.call::<Empty>(Command::Subscribe).await.map(|_| ())
    }

    pub async fn unsubscribe(&self) -> Result<()> {
        self.call::<Empty>(Command::Unsubscribe).await.map(|_| ())
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        Ok(self.call::<Devices>(Command::ListDevices).await?.devices)
    }

    pub async fn rename_device(&self, id: u64, label: &str) -> Result<Device> {
        let label = label.to_owned();
        Ok(self
            .call::<DeviceResult>(Command::RenameDevice { id, label })
            .await?
            .device)
    }

    pub async fn revoke_device(&self, id: u64) -> Result<()> {
        self.call::<Empty>(Command::RevokeDevice { id })
            .await
            .map(|_| ())
    }

    pub async fn set_push_token(&self, token: Option<&str>) -> Result<()> {
        let token = token.map(str::to_owned);
        self.call::<Empty>(Command::SetPushToken { token })
            .await
            .map(|_| ())
    }

    pub async fn create_device_pairing(&self, ttl: Option<Duration>) -> Result<PairingLink> {
        let ttl_secs = ttl.map(|ttl| ttl.as_secs());
        self.call(Command::CreateDevicePairing { ttl_secs }).await
    }

    pub async fn pairings(&self) -> Result<Vec<Pairing>> {
        Ok(self.call::<Pairings>(Command::ListPairings).await?.pairings)
    }

    pub async fn cancel_pairing(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.call::<Empty>(Command::CancelPairing { id })
            .await
            .map(|_| ())
    }

    pub async fn relay_hosts(&self) -> Result<Vec<RelayHost>> {
        Ok(self
            .call::<RelayHosts>(Command::ListRelayHosts)
            .await?
            .relay_hosts)
    }

    pub async fn add_relay_host(&self, host_id: &str, label: &str) -> Result<RelayHost> {
        let (host_id, label) = (host_id.to_owned(), label.to_owned());
        Ok(self
            .call::<RelayHostResult>(Command::AddRelayHost { host_id, label })
            .await?
            .relay_host)
    }

    pub async fn remove_relay_host(&self, host_id: &str) -> Result<()> {
        let host_id = host_id.to_owned();
        self.call::<Empty>(Command::RemoveRelayHost { host_id })
            .await
            .map(|_| ())
    }

    pub async fn create_host_pairing(&self, ttl: Option<Duration>) -> Result<PairingLink> {
        let ttl_secs = ttl.map(|ttl| ttl.as_secs());
        self.call(Command::CreateHostPairing { ttl_secs }).await
    }

    pub async fn hosts(&self) -> Result<Vec<Host>> {
        Ok(self.call::<Hosts>(Command::ListHosts).await?.hosts)
    }

    pub async fn remove_host(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        done(self.call(Command::RemoveHost { name }).await)
    }

    pub async fn set_host_environment(&self, name: &str, environment: Environment) -> Result<Host> {
        let name = name.to_owned();
        Ok(self
            .call::<HostResult>(Command::SetHostEnvironment { name, environment })
            .await?
            .host)
    }

    pub async fn workspaces(&self) -> Result<Vec<Workspace>> {
        Ok(self
            .call::<Workspaces>(Command::ListWorkspaces)
            .await?
            .workspaces)
    }

    pub async fn create_workspace(
        &self,
        name: &str,
        host: &str,
        repositories: Vec<Repository>,
    ) -> Result<Workspace> {
        let (name, host) = (name.to_owned(), host.to_owned());
        Ok(self
            .call::<WorkspaceResult>(Command::CreateWorkspace {
                name,
                host,
                repositories,
            })
            .await?
            .workspace)
    }

    pub async fn update_workspace(
        &self,
        name: &str,
        host: Option<&str>,
        repositories: Option<Vec<Repository>>,
    ) -> Result<Workspace> {
        Ok(self
            .call::<WorkspaceResult>(Command::UpdateWorkspace {
                name: name.to_owned(),
                host: host.map(str::to_owned),
                repositories,
            })
            .await?
            .workspace)
    }

    pub async fn delete_workspace(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        done(self.call(Command::DeleteWorkspace { name }).await)
    }

    pub async fn codespaces(&self) -> Result<Vec<Codespace>> {
        Ok(self
            .call::<Codespaces>(Command::ListCodespaces)
            .await?
            .codespaces)
    }

    pub async fn create_codespace(&self, new: NewCodespace) -> Result<Codespace> {
        Ok(self.create_codespace_with_warnings(new).await?.codespace)
    }

    pub async fn create_codespace_with_warnings(
        &self,
        new: NewCodespace,
    ) -> Result<CodespaceResult> {
        self.call(Command::CreateCodespace {
            workspace: new.workspace,
            preset: new.preset,
            cpus: new.cpus,
            memory_mib: new.memory_mib,
            base_branch: new.base_branch,
            base_branches: new.base_branches,
            client_version: new.client_version,
        })
        .await
    }

    pub async fn close_codespace(&self, id: &str) -> Result<Codespace> {
        let id = id.to_owned();
        Ok(self
            .call::<CodespaceResult>(Command::CloseCodespace { id })
            .await?
            .codespace)
    }

    pub async fn git_providers(&self) -> Result<Vec<GitProvider>> {
        Ok(self
            .call::<GitProviders>(Command::ListGitProviders)
            .await?
            .providers)
    }

    pub async fn set_git_provider(&self, input: GitProviderInput) -> Result<GitProvider> {
        Ok(self
            .call::<GitProviderResult>(Command::SetGitProvider(input))
            .await?
            .provider)
    }

    pub async fn delete_git_provider(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        done(self.call(Command::DeleteGitProvider { name }).await)
    }

    pub async fn git_credentials(&self) -> Result<Vec<GitCredential>> {
        Ok(self
            .call::<GitCredentials>(Command::ListGitCredentials)
            .await?
            .credentials)
    }

    pub async fn set_git_credential(&self, input: GitCredentialInput) -> Result<GitCredential> {
        Ok(self
            .call::<GitCredentialResult>(Command::SetGitCredential(input))
            .await?
            .credential)
    }

    pub async fn check_git_credential(&self, name: &str) -> Result<GitCredential> {
        let name = name.to_owned();
        Ok(self
            .call::<GitCredentialResult>(Command::CheckGitCredential { name })
            .await?
            .credential)
    }

    pub async fn delete_git_credential(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        done(self.call(Command::DeleteGitCredential { name }).await)
    }

    pub async fn presets(&self) -> Result<Vec<Preset>> {
        Ok(self.call::<Presets>(Command::ListPresets).await?.presets)
    }

    pub async fn set_preset(&self, name: &str, yaml: &str) -> Result<Preset> {
        let (name, yaml) = (name.to_owned(), yaml.to_owned());
        Ok(self
            .call::<PresetResult>(Command::SetPreset { name, yaml })
            .await?
            .preset)
    }

    pub async fn delete_preset(&self, name: &str) -> Result<()> {
        let name = name.to_owned();
        done(self.call(Command::DeletePreset { name }).await)
    }

    pub async fn search_repositories(
        &self,
        provider: &str,
        query: &str,
    ) -> Result<RepositorySearch> {
        let (provider, query) = (provider.to_owned(), query.to_owned());
        self.call(Command::SearchRepositories { provider, query })
            .await
    }

    pub async fn branches(
        &self,
        provider: &str,
        owner: &str,
        repository: &str,
    ) -> Result<Branches> {
        self.call(Command::ListBranches {
            provider: provider.to_owned(),
            owner: owner.to_owned(),
            repository: repository.to_owned(),
        })
        .await
    }

    pub async fn git_credential(&self, url: &str) -> Result<GitTokenResult> {
        let url = url.to_owned();
        self.call(Command::GitCredential { url }).await
    }

    pub async fn host_report(&self, report: HostReport) -> Result<()> {
        done(self.call(Command::HostReport { report }).await)
    }

    pub async fn host_instances(&self, instances: Vec<InstanceReport>) -> Result<()> {
        done(self.call(Command::HostInstances { instances }).await)
    }

    pub async fn instance_progress(&self, codespace: &str, step: &str) -> Result<()> {
        let (codespace, step) = (codespace.to_owned(), step.to_owned());
        done(
            self.call(Command::InstanceProgress { codespace, step })
                .await,
        )
    }

    pub async fn bundle(&self) -> Result<Bundle> {
        self.call(Command::CodespaceBundle).await
    }

    pub async fn codespace_agent(&self, address: AgentAddress) -> Result<()> {
        done(self.call(Command::CodespaceAgent { address }).await)
    }

    pub async fn codespace_progress(&self, step: &str) -> Result<()> {
        let step = step.to_owned();
        done(self.call(Command::CodespaceProgress { step }).await)
    }

    pub async fn codespace_ready(&self) -> Result<()> {
        done(self.call(Command::CodespaceReady).await)
    }

    pub async fn codespace_failed(&self, error: &str) -> Result<()> {
        let error = error.to_owned();
        done(self.call(Command::CodespaceFailed { error }).await)
    }
}
