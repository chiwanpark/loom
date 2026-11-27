use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use loom_agent_core::store::{SessionState as StoredState, StoredSession};
use loom_agent_core::tasks::TaskList;
use loom_agent_core::{Agent, AgentNotice, SessionArchive};
use loom_client::OracleClient;
use loom_client::oracle_proto::{
    ArchivedSession, CodespaceMetrics, PresetBundle, Secret, SessionSummary,
};
use loom_llm::{CredentialLock, CredentialSource, Credentials, LlmError};
use loom_proto::{
    ApiError, ArchivedLocation, ArchivedSessionInfo, ContentPart, ErrorCode, SessionId,
    SessionInfo, SessionState,
};
use tokio::sync::{Notify, broadcast, watch};

pub const SECRETS_FILE: &str = "secrets.env";
const RECENT_FETCH: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_secs(30);
const NO_EXPIRY_WAIT: Duration = Duration::from_secs(3600);
const NOTES_DELAY: Duration = Duration::from_secs(1);
const ACTIVITY_CHECK: Duration = Duration::from_secs(10);
const METRICS_REPORT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetPaths {
    pub config: PathBuf,
    pub agents_md: PathBuf,
    pub skills: PathBuf,
    pub env_file: PathBuf,
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot make {}", parent.display()))?;
    }
    let partial = path.with_extension("partial");
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&partial, path)
    };
    write().with_context(|| format!("cannot write {}", path.display()))
}

fn write_if_changed(path: &Path, text: &str) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == text.as_bytes()) {
        return Ok(());
    }
    write_private(path, text.as_bytes())
}

fn remove_file(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("cannot delete {}", path.display())),
    }
}

fn remove_dir(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("cannot delete {}", path.display())),
    }
}

fn write_skills(dir: &Path, bundle: &PresetBundle) -> anyhow::Result<()> {
    let partial = dir.with_extension("partial");
    let old = dir.with_extension("old");
    remove_dir(&partial)?;
    remove_dir(&old)?;
    if bundle.files.is_empty() {
        return remove_dir(dir);
    }
    for file in &bundle.files {
        let relative = file
            .path
            .strip_prefix("skills/")
            .filter(|_| loom_client::oracle_proto::valid_preset_path(&file.path))
            .with_context(|| format!("{:?} is not a path of a skill file", file.path))?;
        let path = partial.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot make {}", parent.display()))?;
        }
        std::fs::write(&path, &file.content)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    if dir.exists() {
        std::fs::rename(dir, &old).with_context(|| format!("cannot move {}", dir.display()))?;
    }
    std::fs::rename(&partial, dir).with_context(|| format!("cannot write {}", dir.display()))?;
    remove_dir(&old)
}

fn quote(value: &str) -> String {
    if !value.contains('\'') {
        return format!("'{value}'");
    }
    let mut quoted = String::from("\"");
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

pub fn env_file_text(secrets: &BTreeMap<String, Secret>) -> String {
    secrets
        .iter()
        .map(|(name, value)| format!("{name}={}\n", quote(value.expose())))
        .collect()
}

pub fn write_preset(paths: &PresetPaths, bundle: &PresetBundle) -> anyhow::Result<()> {
    write_if_changed(&paths.env_file, &env_file_text(&bundle.secrets))?;
    if bundle.agents_md.is_empty() {
        remove_file(&paths.agents_md)?;
    } else {
        write_if_changed(&paths.agents_md, &bundle.agents_md)?;
    }
    write_skills(&paths.skills, bundle)?;
    write_if_changed(&paths.config, &bundle.yaml)
}

#[derive(Default)]
struct Lent {
    credentials: Credentials,
    accounts: BTreeMap<String, String>,
    fetched: Option<Instant>,
}

pub struct OracleCredentials {
    clients: watch::Receiver<OracleClient>,
    state: Mutex<Lent>,
    fetching: tokio::sync::Mutex<()>,
}

struct Snapshot(Credentials);

#[async_trait]
impl CredentialLock for Snapshot {
    async fn load(&self) -> loom_llm::Result<Credentials> {
        Ok(self.0.clone())
    }

    async fn save(&self, _credentials: Credentials) -> loom_llm::Result<()> {
        Err(LlmError::Unsupported(READ_ONLY.into()))
    }
}

const READ_ONLY: &str =
    "the oracle keeps the model accounts of this codespace; sign in or out at the oracle";

impl OracleCredentials {
    pub fn new(clients: watch::Receiver<OracleClient>) -> Arc<Self> {
        Arc::new(Self {
            clients,
            state: Mutex::new(Lent::default()),
            fetching: tokio::sync::Mutex::new(()),
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, Lent> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn fetch(&self) -> anyhow::Result<Option<u64>> {
        let _fetching = self.fetching.lock().await;
        let client = self.clients.borrow().clone();
        let lent = client
            .model_credentials()
            .await
            .context("cannot get the model credentials from the oracle")?;
        for error in &lent.errors {
            tracing::warn!(%error, "the oracle cannot lend a model credential");
        }
        let mut credentials = Credentials::default();
        for (provider, section) in lent.sections {
            credentials.set_section(&provider, Some(section));
        }
        *self.lock_state() = Lent {
            credentials,
            accounts: lent.accounts,
            fetched: Some(Instant::now()),
        };
        Ok(lent.valid_secs)
    }

    pub async fn keep_fresh(self: Arc<Self>, mut wait: Duration, wake: Arc<Notify>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = wake.notified() => {}
            }
            wait = match self.fetch().await {
                Ok(Some(valid)) => Duration::from_secs(valid),
                Ok(None) => NO_EXPIRY_WAIT,
                Err(err) => {
                    tracing::warn!(error = %format!("{err:#}"), "cannot refresh the model credentials");
                    RETRY
                }
            };
        }
    }
}

pub fn first_wait(fetched: &anyhow::Result<Option<u64>>) -> Duration {
    match fetched {
        Ok(Some(valid)) => Duration::from_secs(*valid),
        Ok(None) => NO_EXPIRY_WAIT,
        Err(_) => RETRY,
    }
}

#[async_trait]
impl CredentialSource for OracleCredentials {
    fn location(&self) -> PathBuf {
        PathBuf::from("the oracle")
    }

    async fn load(&self) -> loom_llm::Result<Credentials> {
        Ok(self.lock_state().credentials.clone())
    }

    async fn lock(&self) -> loom_llm::Result<Box<dyn CredentialLock>> {
        let recent = self
            .lock_state()
            .fetched
            .is_some_and(|at| at.elapsed() < RECENT_FETCH);
        if !recent && let Err(err) = self.fetch().await {
            return Err(LlmError::NotAuthenticated {
                provider: "oracle".into(),
                message: format!("{err:#}"),
            });
        }
        Ok(Box::new(Snapshot(self.lock_state().credentials.clone())))
    }

    fn read_only(&self) -> Option<String> {
        Some(READ_ONLY.into())
    }

    async fn account_names(&self) -> BTreeMap<String, String> {
        self.lock_state().accounts.clone()
    }
}

fn state_name(state: SessionState) -> String {
    let mut name = String::new();
    for (i, c) in format!("{state:?}").chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                name.push('_');
            }
            name.push(c.to_ascii_lowercase());
        } else {
            name.push(c);
        }
    }
    name
}

pub fn summary(info: &SessionInfo, updated_at: u64) -> SessionSummary {
    SessionSummary {
        id: info.id.to_string(),
        title: info.title.clone(),
        state: state_name(info.state),
        archived: info.archived,
        model: info.model.clone(),
        created_at: info.created_at,
        updated_at: updated_at.max(info.created_at),
    }
}

fn now_unix() -> u64 {
    loom_llm::now_unix()
}

async fn send_all(agent: &Agent, client: &OracleClient) {
    let now = now_unix();
    let sessions = agent
        .list_sessions()
        .iter()
        .map(|info| summary(info, now))
        .collect();
    if let Err(err) = client.session_summaries(sessions, Vec::new(), true).await {
        tracing::warn!(error = %err, "cannot send the session summaries to the oracle");
    }
}

pub fn codespace_metrics(agent: &Agent) -> CodespaceMetrics {
    let snapshot = agent.metrics_snapshot();
    CodespaceMetrics {
        uptime_secs: snapshot.uptime_secs,
        sessions: snapshot.sessions,
        busy_sessions: snapshot.busy_sessions,
        connections: snapshot.connections,
        terminals: agent.open_terminals(),
        turns_completed: snapshot.turns_completed,
        turns_failed: snapshot.turns_failed,
        tool_calls: snapshot.tool_calls,
        input_tokens: snapshot.input_tokens,
        output_tokens: snapshot.output_tokens,
    }
}

pub fn is_active(metrics: &CodespaceMetrics) -> bool {
    metrics.connections > 0 || metrics.busy_sessions > 0 || metrics.terminals > 0
}

pub async fn report_activity(agent: Agent, mut clients: watch::Receiver<OracleClient>) {
    let mut reported: Option<bool> = None;
    let mut last_report = Instant::now();
    let mut check = tokio::time::interval(ACTIVITY_CHECK);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = clients.changed() => {
                if changed.is_err() {
                    return;
                }
                reported = None;
            }
            _ = check.tick() => {}
        }
        let metrics = codespace_metrics(&agent);
        let active = is_active(&metrics);
        if reported == Some(active) && last_report.elapsed() < METRICS_REPORT {
            continue;
        }
        let client = clients.borrow_and_update().clone();
        if let Err(err) = client.codespace_activity(active, Some(metrics)).await {
            tracing::debug!(error = %err, "cannot report the activity to the oracle");
        }
        reported = Some(active);
        last_report = Instant::now();
    }
}

pub async fn report_sessions(agent: Agent, mut clients: watch::Receiver<OracleClient>) {
    let mut notices = agent.notices();
    let client = clients.borrow_and_update().clone();
    send_all(&agent, &client).await;
    loop {
        tokio::select! {
            changed = clients.changed() => {
                if changed.is_err() {
                    return;
                }
                let client = clients.borrow_and_update().clone();
                send_all(&agent, &client).await;
            }
            notice = notices.recv() => {
                let client = clients.borrow().clone();
                let sent = match notice {
                    Ok(AgentNotice::SessionUpdated(info)) => {
                        client
                            .session_summaries(vec![summary(&info, now_unix())], Vec::new(), false)
                            .await
                    }
                    Ok(AgentNotice::SessionRemoved(id)) => {
                        client
                            .session_summaries(Vec::new(), vec![id.to_string()], false)
                            .await
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        send_all(&agent, &client).await;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if let Err(err) = sent {
                    tracing::warn!(error = %err, "cannot send a session summary to the oracle");
                }
            }
        }
    }
}

pub async fn report_notes(
    agent: Agent,
    clients: watch::Receiver<OracleClient>,
    mut sent: Option<String>,
) {
    let mut notices = agent.notices();
    loop {
        match notices.recv().await {
            Ok(AgentNotice::NotesChanged { .. }) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Ok(_) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
        tokio::time::sleep(NOTES_DELAY).await;
        while !notices.is_empty() {
            if let Err(broadcast::error::TryRecvError::Closed) = notices.try_recv() {
                return;
            }
        }
        let text = agent.notes();
        if sent.as_deref() == Some(text.as_str()) {
            continue;
        }
        let client = clients.borrow().clone();
        match client.codespace_notes(&text).await {
            Ok(()) => sent = Some(text),
            Err(err) => tracing::warn!(error = %err, "cannot send the notes to the oracle"),
        }
    }
}

pub async fn start_first_message(agent: &Agent, client: &OracleClient) {
    let message = match client.take_first_message().await {
        Ok(Some(message)) => message,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(error = %err, "cannot get the first message from the oracle");
            return;
        }
    };
    let session = match agent.create_session(message.model.as_deref()) {
        Ok(session) => session,
        Err(err) => {
            tracing::warn!(error = %err, model = ?message.model, "cannot use the model of the first message");
            match agent.create_session(None) {
                Ok(session) => session,
                Err(err) => {
                    tracing::error!(error = %err, "cannot start a session for the first message");
                    return;
                }
            }
        }
    };
    match session.send_prompt(vec![ContentPart::text(message.text)]) {
        Ok(()) => tracing::info!(session = %session.id(), "started the first message"),
        Err(err) => tracing::error!(error = %err, "cannot send the first message"),
    }
}

pub struct OracleArchive {
    clients: watch::Receiver<OracleClient>,
}

impl OracleArchive {
    pub fn new(clients: watch::Receiver<OracleClient>) -> Arc<Self> {
        Arc::new(Self { clients })
    }

    fn client(&self) -> OracleClient {
        self.clients.borrow().clone()
    }
}

fn oracle_error(err: loom_client::ClientError) -> ApiError {
    match &err {
        loom_client::ClientError::Oracle(oracle) => {
            let code = match oracle.code {
                loom_client::oracle_proto::ErrorCode::NotFound => ErrorCode::NotFound,
                loom_client::oracle_proto::ErrorCode::BadRequest => ErrorCode::BadRequest,
                loom_client::oracle_proto::ErrorCode::Conflict => ErrorCode::Conflict,
                _ => ErrorCode::Internal,
            };
            ApiError::new(code, oracle.message.clone())
        }
        _ => ApiError::new(
            ErrorCode::Internal,
            format!("cannot reach the oracle: {err}"),
        ),
    }
}

fn archived_info(session: ArchivedSession) -> ArchivedSessionInfo {
    ArchivedSessionInfo {
        id: SessionId(session.id),
        title: session.title,
        model: session.model,
        created_at: session.created_at,
        updated_at: session.updated_at,
        location: ArchivedLocation::Codespace {
            codespace: session.codespace,
            branch: session.branch,
        },
    }
}

#[async_trait]
impl SessionArchive for OracleArchive {
    async fn list(&self) -> Result<Vec<ArchivedSessionInfo>, ApiError> {
        let sessions = self
            .client()
            .archived_sessions()
            .await
            .map_err(oracle_error)?;
        Ok(sessions.into_iter().map(archived_info).collect())
    }

    async fn fetch(&self, codespace: &str, session: &SessionId) -> Result<StoredSession, ApiError> {
        let client = self.client();
        let data = client
            .archived_session(codespace, &session.0)
            .await
            .map_err(oracle_error)?;
        let state: StoredState = serde_json::from_slice(&data.state).map_err(|err| {
            ApiError::new(
                ErrorCode::Internal,
                format!("cannot read the archived session: {err}"),
            )
        })?;
        let tasks: TaskList = data
            .tasks
            .as_deref()
            .and_then(|tasks| serde_json::from_str(tasks).ok())
            .unwrap_or_default();
        let events = client
            .archived_events(codespace, &session.0)
            .await
            .map_err(oracle_error)?;
        Ok(StoredSession {
            id: session.clone(),
            created_at: data.session.created_at,
            updated_at: data.session.updated_at,
            state,
            events,
            notes: String::new(),
            tasks,
        })
    }

    async fn resumed(&self, codespace: &str, session: &SessionId) -> Result<(), ApiError> {
        self.client()
            .mark_session_resumed(codespace, &session.0)
            .await
            .map_err(oracle_error)
    }
}

#[cfg(test)]
mod tests {
    use loom_client::oracle_proto::PresetFile;

    use super::*;

    #[test]
    fn clients_running_turns_and_terminals_make_a_codespace_active() {
        assert!(!is_active(&CodespaceMetrics::default()));
        for metrics in [
            CodespaceMetrics {
                connections: 1,
                ..Default::default()
            },
            CodespaceMetrics {
                busy_sessions: 1,
                ..Default::default()
            },
            CodespaceMetrics {
                terminals: 1,
                ..Default::default()
            },
        ] {
            assert!(is_active(&metrics), "{metrics:?}");
        }
        let idle = CodespaceMetrics {
            sessions: 4,
            turns_completed: 9,
            ..Default::default()
        };
        assert!(!is_active(&idle));
    }

    #[test]
    fn secrets_keep_their_values_in_the_env_file() {
        let secrets: BTreeMap<String, Secret> = [
            ("PLAIN", "abc123"),
            ("QUOTE", "it's \"x\" $HOME \\n"),
            ("SPACES", "a b  c"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), Secret::new(v)))
        .collect();
        let parsed = loom_tools::env::parse_dotenv(&env_file_text(&secrets)).unwrap();
        let parsed: BTreeMap<String, String> = parsed.into_iter().collect();
        for (name, value) in &secrets {
            assert_eq!(parsed[name], value.expose(), "{name}");
        }
    }

    #[test]
    fn a_preset_becomes_files() {
        let dir = tempfile::tempdir().unwrap();
        let paths = PresetPaths {
            config: dir.path().join("config/agent.yaml"),
            agents_md: dir.path().join("config/AGENTS.md"),
            skills: dir.path().join("config/skills"),
            env_file: dir.path().join("data/secrets.env"),
        };
        let mut bundle = PresetBundle {
            name: "work".into(),
            yaml: "llm: {}\n".into(),
            agents_md: "RULES".into(),
            files: vec![
                PresetFile {
                    path: "skills/review/SKILL.md".into(),
                    content: "review".into(),
                },
                PresetFile {
                    path: "skills/review/scripts/run.sh".into(),
                    content: "echo".into(),
                },
            ],
            secrets: [("TOKEN".to_owned(), Secret::new("t"))]
                .into_iter()
                .collect(),
            accounts: BTreeMap::new(),
        };
        write_preset(&paths, &bundle).unwrap();
        assert_eq!(std::fs::read_to_string(&paths.agents_md).unwrap(), "RULES");
        assert_eq!(
            std::fs::read_to_string(paths.skills.join("review/scripts/run.sh")).unwrap(),
            "echo"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.env_file).unwrap(),
            "TOKEN='t'\n"
        );
        let mode = std::fs::metadata(&paths.env_file).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );

        bundle.agents_md.clear();
        bundle.files.truncate(1);
        bundle.files[0].path = "skills/other/SKILL.md".into();
        write_preset(&paths, &bundle).unwrap();
        assert!(!paths.agents_md.exists());
        assert!(!paths.skills.join("review").exists());
        assert!(paths.skills.join("other/SKILL.md").is_file());

        bundle.files.clear();
        write_preset(&paths, &bundle).unwrap();
        assert!(!paths.skills.exists());
    }

    #[test]
    fn states_have_snake_case_names() {
        assert_eq!(state_name(SessionState::Idle), "idle");
        assert_eq!(state_name(SessionState::RunningTool), "running_tool");
    }
}
