use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use loom_config::{AgentConfig, Dirs};
use loom_github::{
    ApiRequest, ApiResponse, Direct, GitHub, GitHubError, Page, Transport, default_api_url,
    repository_path,
};
use loom_oracle_proto::{
    Branches, CheckResult, ErrorCode, Event, GhLogin, GitCredential, GitCredentialInput,
    GitProvider, GitProviderInput, GitProviderSetup, GitSetup, GitToken, GitTokenResult, HostCall,
    OracleError, Preset, ProviderEndpoint, ProviderRequest, ProviderRoute, RemoteRepository,
    Repository, RepositorySearch, Secret, WorkspaceCheck, WorkspaceCheckResult,
    repository_directory, valid_directory, valid_name,
};

use super::{Oracle, bad_request, not_found};
use crate::store::{CredentialCheck, CredentialRow, PresetRow, ProviderRow};

pub(super) const REACH_TTL: Duration = Duration::from_secs(600);
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PRESET: usize = 256 * 1024;
const MAX_TOKEN: usize = 1000;
const MAX_REPOSITORIES: usize = 20;
const SEARCH_LIMIT: u32 = 30;
const MAX_FOUND: usize = 50;
const BRANCH_LIMIT: u32 = 100;
const PRESET_HOME: &str = "/home/dev";
const GITHUB_HOST: &str = "github.com";
const TOKEN_USER: &str = "x-access-token";

pub(super) type ReachCache = HashMap<u64, HashMap<String, (Instant, Option<String>)>>;

fn unavailable(message: impl Into<String>) -> OracleError {
    OracleError::new(ErrorCode::Unavailable, message)
}

pub fn normalize_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split('/').next().unwrap_or_default();
    let valid = !host.is_empty()
        && url.len() <= 500
        && url.chars().all(|c| c.is_ascii_graphic())
        && !url.contains(['?', '#', '@', '\\'])
        && !rest.contains("//")
        && !rest.split('/').any(|part| part == "..");
    valid.then(|| url.to_owned())
}

fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', ':']).next().unwrap_or_default()
}

pub fn provider_path(web_url: &str, url: &str) -> Option<(String, String)> {
    loom_github::repository_of(web_url, url)
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

fn rank(credential: &CredentialRow) -> u8 {
    match (&credential.owner, &credential.repository) {
        (Some(_), Some(_)) => 0,
        (Some(_), None) => 1,
        _ => 2,
    }
}

pub fn select_credential<'a>(
    credentials: &'a [CredentialRow],
    provider: u64,
    owner: &str,
    repository: &str,
) -> Option<&'a CredentialRow> {
    credentials
        .iter()
        .filter(|c| c.provider == provider)
        .filter(|c| match (&c.owner, &c.repository) {
            (Some(o), Some(r)) => {
                o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repository)
            }
            (Some(o), None) => o.eq_ignore_ascii_case(owner),
            _ => true,
        })
        .min_by_key(|c| rank(c))
}

fn widest(credentials: &[CredentialRow], provider: u64) -> Option<&CredentialRow> {
    credentials
        .iter()
        .filter(|c| c.provider == provider)
        .min_by_key(|c| 2 - rank(c))
}

pub(super) fn git_provider(row: &ProviderRow) -> GitProvider {
    GitProvider {
        name: row.name.clone(),
        web_url: row.web_url.clone(),
        api_url: row.api_url.clone(),
        git_name: row.git_name.clone(),
        git_email: row.git_email.clone(),
        route: row.route,
        created_at: row.created_at,
    }
}

pub(super) fn git_credential(row: &CredentialRow) -> GitCredential {
    GitCredential {
        name: row.name.clone(),
        provider: row.provider_name.clone(),
        owner: row.owner.clone(),
        repository: row.repository.clone(),
        login: row.login.clone(),
        expires_at: row.expires_at,
        checked_at: row.checked_at,
        check_error: row.check_error.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub(super) fn preset(row: &PresetRow) -> Preset {
    Preset {
        name: row.name.clone(),
        yaml: row.yaml.clone(),
        git_credentials: row.credentials.clone(),
        agents_md: row.agents_md.clone(),
        files: row.files.clone(),
        model_accounts: row
            .accounts
            .iter()
            .map(|(provider, _, name)| (provider.clone(), name.clone()))
            .collect(),
        secrets: row
            .secrets
            .iter()
            .map(|(variable, _, name)| (variable.clone(), name.clone()))
            .collect(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn remote(repository: loom_github::RemoteRepository) -> RemoteRepository {
    RemoteRepository {
        url: repository.clone_url,
        web_url: repository.web_url,
        owner: repository.owner,
        name: repository.name,
        description: repository.description,
        private: repository.private,
        default_branch: repository.default_branch,
    }
}

fn github_error(err: GitHubError) -> OracleError {
    match err {
        GitHubError::Status { status: 401, .. } => OracleError::new(
            ErrorCode::Forbidden,
            format!("the Git provider refused the token: {err}"),
        ),
        GitHubError::Status {
            status: 403 | 404, ..
        } => OracleError::new(
            ErrorCode::NotFound,
            format!("the Git provider did not find it or refused it: {err}"),
        ),
        GitHubError::Status { .. } => OracleError::new(ErrorCode::Conflict, err.to_string()),
        other => unavailable(other.to_string()),
    }
}

pub fn preset_dirs() -> Dirs {
    let home = std::path::PathBuf::from(PRESET_HOME);
    Dirs {
        config_home: home.join(".config"),
        data_home: home.join(".local/share"),
        state_home: home.join(".local/state"),
        runtime_dir: None,
        home,
    }
}

pub fn parse_preset(name: &str, yaml: &str) -> Result<AgentConfig, OracleError> {
    if yaml.len() > MAX_PRESET {
        return Err(bad_request(format!(
            "the YAML file of a preset has up to {MAX_PRESET} bytes"
        )));
    }
    AgentConfig::parse_preset(yaml, &format!("preset {name}"), &preset_dirs())
        .map_err(|err| bad_request(err.to_string()))
}

struct HostTransport {
    oracle: Arc<Oracle>,
    provider: String,
    api_url: String,
    token: Option<Secret>,
}

#[async_trait]
impl Transport for HostTransport {
    async fn send(&self, request: &ApiRequest) -> Result<ApiResponse, GitHubError> {
        let route = |message: String| GitHubError::Route(message);
        let host = self
            .oracle
            .host_for_provider(&self.provider, &self.api_url)
            .await
            .map_err(|err| route(err.message))?;
        let call = HostCall::ProviderRequest(Box::new(ProviderRequest {
            api_url: self.api_url.clone(),
            token: self.token.clone(),
            method: request.method.as_str().to_owned(),
            path: request.path.clone(),
            query: request.query.clone(),
            body: request.body.clone(),
        }));
        let value = self
            .oracle
            .call_host(host, call, PROVIDER_TIMEOUT)
            .await
            .map_err(|err| route(format!("the host cannot send the request: {err}")))?;
        serde_json::from_value(value).map_err(|err| GitHubError::Decode(err.to_string()))
    }
}

impl Oracle {
    fn lock_reach(&self) -> std::sync::MutexGuard<'_, ReachCache> {
        self.reach.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn host_reach(&self, host: u64) -> Vec<CheckResult> {
        let mut results: Vec<CheckResult> = self
            .lock_reach()
            .get(&host)
            .map(|providers| {
                providers
                    .iter()
                    .map(|(name, (_, error))| CheckResult {
                        name: name.clone(),
                        error: error.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    fn forget_reach(&self, provider: &str) {
        for providers in self.lock_reach().values_mut() {
            providers.remove(provider);
        }
    }

    fn provider_row(&self, name: &str) -> Result<ProviderRow, OracleError> {
        self.store
            .provider_by_name(name)?
            .ok_or_else(|| not_found(format!("no Git provider has the name {name}")))
    }

    pub(super) async fn reach(
        &self,
        host: u64,
        providers: &[ProviderRow],
    ) -> Result<HashMap<String, Option<String>>, OracleError> {
        let mut results = HashMap::new();
        let mut missing = Vec::new();
        {
            let cache = self.lock_reach();
            for provider in providers {
                match cache.get(&host).and_then(|c| c.get(&provider.name)) {
                    Some((at, error)) if at.elapsed() < REACH_TTL => {
                        results.insert(provider.name.clone(), error.clone());
                    }
                    _ => missing.push(ProviderEndpoint {
                        name: provider.name.clone(),
                        api_url: provider.api_url.clone(),
                    }),
                }
            }
        }
        if missing.is_empty() {
            return Ok(results);
        }
        let value = self
            .call_host(
                host,
                HostCall::CheckWorkspace(WorkspaceCheck {
                    providers: missing.clone(),
                    directories: Vec::new(),
                }),
                CHECK_TIMEOUT,
            )
            .await?;
        let checked: WorkspaceCheckResult = serde_json::from_value(value).map_err(|err| {
            OracleError::new(
                ErrorCode::Internal,
                format!("the host sent a malformed check: {err}"),
            )
        })?;
        {
            let now = Instant::now();
            let mut cache = self.lock_reach();
            let entry = cache.entry(host).or_default();
            for endpoint in &missing {
                let error = checked
                    .providers
                    .iter()
                    .find(|r| r.name == endpoint.name)
                    .map_or_else(
                        || Some("the host did not check the provider".to_owned()),
                        |r| r.error.clone(),
                    );
                entry.insert(endpoint.name.clone(), (now, error.clone()));
                results.insert(endpoint.name.clone(), error);
            }
        }
        self.host_changed(host);
        Ok(results)
    }

    pub(super) fn refresh_reach(self: &Arc<Self>, host: u64) {
        let oracle = self.clone();
        tokio::spawn(async move {
            let providers = match oracle.store.providers() {
                Ok(providers) if !providers.is_empty() => providers,
                Ok(_) => return,
                Err(err) => {
                    tracing::error!(error = %err, "cannot read the Git providers");
                    return;
                }
            };
            if let Err(err) = oracle.reach(host, &providers).await {
                tracing::debug!(host, error = %err, "cannot check the reach of a host");
            }
        });
    }

    async fn host_for_provider(&self, provider: &str, api_url: &str) -> Result<u64, OracleError> {
        let mut hosts: Vec<u64> = self.lock_links().keys().copied().collect();
        hosts.sort_unstable();
        let row = ProviderRow {
            id: 0,
            name: provider.to_owned(),
            web_url: String::new(),
            api_url: api_url.to_owned(),
            git_name: None,
            git_email: None,
            route: ProviderRoute::Hosts,
            created_at: 0,
        };
        let fresh = |host: &u64| {
            self.lock_reach()
                .get(host)
                .and_then(|c| c.get(provider))
                .is_some_and(|(at, error)| error.is_none() && at.elapsed() < REACH_TTL)
        };
        if let Some(host) = hosts.iter().find(|host| fresh(host)) {
            return Ok(*host);
        }
        for host in hosts {
            if let Ok(results) = self.reach(host, std::slice::from_ref(&row)).await
                && results.get(provider).is_some_and(Option::is_none)
            {
                return Ok(host);
            }
        }
        Err(unavailable(format!(
            "no connected host reaches the Git provider {provider}"
        )))
    }

    fn github(self: &Arc<Self>, provider: &ProviderRow, token: Option<Secret>) -> GitHub {
        match provider.route {
            ProviderRoute::Hosts => GitHub::new(Arc::new(HostTransport {
                oracle: self.clone(),
                provider: provider.name.clone(),
                api_url: provider.api_url.clone(),
                token,
            })),
            _ => GitHub::new(Arc::new(Direct::new(
                self.http.clone(),
                &provider.api_url,
                token.map(|t| t.expose().to_owned()),
            ))),
        }
    }

    fn token(&self, credential: &CredentialRow) -> Result<Secret, OracleError> {
        Ok(Secret::new(
            self.store.credential_token(credential.id)?.as_str(),
        ))
    }

    pub(super) fn git_changed(&self) {
        self.emit(Event::GitChanged);
    }

    pub fn git_providers(&self) -> Result<Vec<GitProvider>, OracleError> {
        Ok(self.store.providers()?.iter().map(git_provider).collect())
    }

    pub fn set_git_provider(
        self: &Arc<Self>,
        input: &GitProviderInput,
        by: Option<u64>,
    ) -> Result<GitProvider, OracleError> {
        let name = &input.name;
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not a provider name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        let web_url = normalize_url(&input.web_url).ok_or_else(|| {
            bad_request(format!(
                "{:?} is not an HTTPS URL of a server",
                input.web_url
            ))
        })?;
        let api_url = match &input.api_url {
            Some(url) => normalize_url(url)
                .ok_or_else(|| bad_request(format!("{url:?} is not an HTTPS URL of an API")))?,
            None => default_api_url(&web_url),
        };
        if let Some(git_name) = &input.git_name
            && !valid_text(git_name, 100)
        {
            return Err(bad_request("the Git name is one line of up to 100 bytes"));
        }
        if let Some(email) = &input.git_email
            && (!valid_text(email, 200) || !email.contains('@') || email.contains(' '))
        {
            return Err(bad_request(format!("{email:?} is not an email address")));
        }
        if input.route == ProviderRoute::Unknown {
            return Err(bad_request("the route is direct or hosts"));
        }
        let old = self.store.provider_by_name(name)?;
        let row = self.store.put_provider(&ProviderRow {
            id: 0,
            name: name.clone(),
            web_url,
            api_url,
            git_name: input.git_name.clone(),
            git_email: input.git_email.clone(),
            route: input.route,
            created_at: 0,
        })?;
        if old.as_ref().is_none_or(|old| old.api_url != row.api_url) {
            self.forget_reach(name);
            let hosts: Vec<u64> = self.lock_links().keys().copied().collect();
            for host in hosts {
                self.refresh_reach(host);
            }
        }
        self.audit(by, "git_provider_set", &format!("provider {name}"));
        let provider = git_provider(&row);
        self.emit(Event::GitProviderChanged {
            provider: provider.clone(),
        });
        self.git_changed();
        Ok(provider)
    }

    pub fn delete_git_provider(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        if !self.store.delete_provider(name)? {
            return Err(not_found(format!("no Git provider has the name {name}")));
        }
        self.forget_reach(name);
        self.audit(by, "git_provider_deleted", &format!("provider {name}"));
        self.emit(Event::GitProviderRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    pub fn git_credentials(&self) -> Result<Vec<GitCredential>, OracleError> {
        Ok(self
            .store
            .credentials()?
            .iter()
            .map(git_credential)
            .collect())
    }

    pub async fn set_git_credential(
        self: &Arc<Self>,
        input: GitCredentialInput,
        by: Option<u64>,
    ) -> Result<GitCredential, OracleError> {
        let name = &input.name;
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not a credential name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        let provider = self.provider_row(&input.provider)?;
        match (&input.owner, &input.repository) {
            (None, Some(_)) => {
                return Err(bad_request(
                    "a credential for a repository also needs its owner",
                ));
            }
            (Some(owner), repository) => {
                let path = format!("{owner}/{}", repository.as_deref().unwrap_or("x"));
                if repository_path(&path).is_none() {
                    return Err(bad_request(format!(
                        "{path:?} is not an owner and a repository of GitHub"
                    )));
                }
            }
            (None, None) => {}
        }
        if let Some(token) = &input.token {
            let token = token.expose();
            if token.is_empty()
                || token.len() > MAX_TOKEN
                || !token.chars().all(|c| c.is_ascii_graphic())
            {
                return Err(bad_request(format!(
                    "a token is 1 to {MAX_TOKEN} printable ASCII characters"
                )));
            }
        }
        let row = self.store.put_credential(
            name,
            provider.id,
            input.owner.as_deref(),
            input.repository.as_deref(),
            input.token.as_ref().map(Secret::expose),
        )?;
        self.audit(
            by,
            "git_credential_set",
            &format!(
                "credential {name} for {} ({}){}",
                provider.name,
                git_credential(&row).scope(),
                if input.token.is_some() {
                    " with a new token"
                } else {
                    ""
                }
            ),
        );
        if row.checked_at.is_none() {
            return self.check_git_credential(name).await;
        }
        self.git_changed();
        let credential = git_credential(&row);
        self.emit(Event::GitCredentialChanged {
            credential: credential.clone(),
        });
        Ok(credential)
    }

    pub async fn check_git_credential(
        self: &Arc<Self>,
        name: &str,
    ) -> Result<GitCredential, OracleError> {
        let row = self
            .store
            .credential_by_name(name)?
            .ok_or_else(|| not_found(format!("no Git credential has the name {name}")))?;
        let provider = self.provider_row(&row.provider_name)?;
        let github = self.github(&provider, Some(self.token(&row)?));
        let check = match github.user().await {
            Ok(user) => CredentialCheck {
                login: Some(user.login),
                user_id: Some(user.id),
                expires_at: user.expires_at,
                error: None,
            },
            Err(err) => CredentialCheck {
                login: row.login.clone(),
                user_id: row.user_id,
                expires_at: row.expires_at,
                error: Some(err.to_string()),
            },
        };
        self.store.set_credential_check(row.id, &check)?;
        let row = self
            .store
            .credential_by_name(name)?
            .ok_or_else(|| not_found(format!("no Git credential has the name {name}")))?;
        self.git_changed();
        let credential = git_credential(&row);
        self.emit(Event::GitCredentialChanged {
            credential: credential.clone(),
        });
        Ok(credential)
    }

    pub fn delete_git_credential(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        if !self.store.delete_credential(name)? {
            return Err(not_found(format!("no Git credential has the name {name}")));
        }
        self.audit(by, "git_credential_deleted", &format!("credential {name}"));
        self.emit(Event::GitCredentialRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    pub fn presets(&self) -> Result<Vec<Preset>, OracleError> {
        Ok(self.store.presets()?.iter().map(preset).collect())
    }

    pub(super) fn preset_git_credentials(
        &self,
        name: &str,
        config: &AgentConfig,
    ) -> Result<Vec<u64>, OracleError> {
        let mut ids = Vec::new();
        for credential in &config.git.credentials {
            let row = self.store.credential_by_name(credential)?.ok_or_else(|| {
                bad_request(format!(
                    "preset {name}: git.credentials names {credential:?}, but the oracle has no such Git credential"
                ))
            })?;
            if ids.contains(&row.id) {
                return Err(bad_request(format!(
                    "preset {name}: git.credentials names {credential:?} twice"
                )));
            }
            ids.push(row.id);
        }
        Ok(ids)
    }

    pub fn delete_preset(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        if !self.store.delete_preset(name)? {
            return Err(not_found(format!("no preset has the name {name}")));
        }
        self.audit(by, "preset_deleted", &format!("preset {name}"));
        self.emit(Event::PresetRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    pub(super) fn preset_by_name(&self, name: &str) -> Result<PresetRow, OracleError> {
        self.store
            .preset_by_name(name)?
            .ok_or_else(|| not_found(format!("no preset has the name {name}")))
    }

    pub(super) fn resolve_repositories(
        &self,
        inputs: &[Repository],
    ) -> Result<Vec<Repository>, OracleError> {
        if inputs.is_empty() || inputs.len() > MAX_REPOSITORIES {
            return Err(bad_request(format!(
                "a workspace has 1 to {MAX_REPOSITORIES} repositories"
            )));
        }
        let providers = self.store.providers()?;
        let mut resolved: Vec<Repository> = Vec::new();
        for input in inputs {
            let repository = resolve_repository(input, &providers)?;
            if resolved.iter().any(|r| r.directory == repository.directory) {
                return Err(bad_request(format!(
                    "two repositories have the directory {}; set a different directory for one",
                    repository.directory.as_deref().unwrap_or_default()
                )));
            }
            resolved.push(repository);
        }
        Ok(resolved)
    }

    pub(super) fn repository_providers(
        &self,
        repositories: &[Repository],
    ) -> Result<Vec<ProviderRow>, OracleError> {
        let mut rows: Vec<ProviderRow> = Vec::new();
        for name in repositories.iter().filter_map(|r| r.provider.as_deref()) {
            if !rows.iter().any(|row| row.name == name) {
                rows.push(self.provider_row(name)?);
            }
        }
        Ok(rows)
    }

    pub(super) async fn check_reach(
        &self,
        host: u64,
        host_name: &str,
        repositories: &[Repository],
        required: bool,
    ) -> Result<(), OracleError> {
        let providers = self.repository_providers(repositories)?;
        if providers.is_empty() {
            return Ok(());
        }
        if !self.host_connected(host) {
            return if required {
                Err(unavailable(format!(
                    "the host {host_name} is not connected"
                )))
            } else {
                Ok(())
            };
        }
        let results = match self.reach(host, &providers).await {
            Ok(results) => results,
            Err(err) if err.code == ErrorCode::UnknownCommand => {
                return Err(unavailable(format!(
                    "the host {host_name} cannot check the Git providers; update loom-host"
                )));
            }
            Err(err) => return Err(err),
        };
        for provider in &providers {
            if let Some(Some(error)) = results.get(&provider.name) {
                return Err(unavailable(format!(
                    "the host {host_name} cannot reach the Git provider {}: {error}",
                    provider.name
                )));
            }
        }
        Ok(())
    }

    pub(super) fn credential_warnings(
        &self,
        repositories: &[Repository],
        preset: Option<&PresetRow>,
    ) -> Result<Vec<String>, OracleError> {
        let providers = self.repository_providers(repositories)?;
        let credentials = match preset {
            Some(preset) => self.store.preset_credentials(preset.id)?,
            None => Vec::new(),
        };
        let mut warnings = Vec::new();
        for repository in repositories {
            let Some(provider) = providers
                .iter()
                .find(|p| Some(&p.name) == repository.provider.as_ref())
            else {
                continue;
            };
            let Some((owner, name)) = provider_path(&provider.web_url, &repository.url) else {
                continue;
            };
            if select_credential(&credentials, provider.id, &owner, &name).is_none() {
                let preset =
                    preset.map_or("no preset".to_owned(), |p| format!("the preset {}", p.name));
                warnings.push(format!(
                    "{preset} has no Git credential for {owner}/{name} on {}; the codespace can only read it if it is public",
                    provider.name
                ));
            }
        }
        for provider in &providers {
            let chosen = main_credential(provider, repositories, &credentials);
            if identity(provider, chosen).1.is_none() {
                warnings.push(format!(
                    "the Git provider {} has no Git email; set one, or commits in its clones fail",
                    provider.name
                ));
            }
        }
        Ok(warnings)
    }

    pub(super) fn git_setup(
        &self,
        preset: Option<u64>,
        repositories: &[Repository],
    ) -> Result<GitSetup, OracleError> {
        let all = self.store.providers()?;
        let credentials = match preset {
            Some(preset) => self.store.preset_credentials(preset)?,
            None => Vec::new(),
        };
        let mut names: Vec<&str> = Vec::new();
        for name in repositories
            .iter()
            .filter_map(|r| r.provider.as_deref())
            .chain(credentials.iter().map(|c| c.provider_name.as_str()))
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let mut setup = GitSetup::default();
        for name in names {
            let Some(provider) = all.iter().find(|p| p.name == name) else {
                continue;
            };
            let chosen = main_credential(provider, repositories, &credentials);
            let (git_name, git_email) = identity(provider, chosen);
            let gh = match chosen {
                Some(credential) => Some(GhLogin {
                    user: credential
                        .login
                        .clone()
                        .unwrap_or_else(|| TOKEN_USER.to_owned()),
                    token: self.token(credential)?,
                }),
                None => None,
            };
            setup.providers.push(GitProviderSetup {
                name: provider.name.clone(),
                web_url: provider.web_url.clone(),
                api_url: provider.api_url.clone(),
                git_name,
                git_email,
                credentials: credentials.iter().any(|c| c.provider == provider.id),
                gh,
            });
        }
        Ok(setup)
    }

    pub fn git_token(&self, peer: u64, url: &str) -> Result<GitTokenResult, OracleError> {
        let row = self.codespace_of_peer(peer)?;
        let Some(preset) = row.preset else {
            return Ok(GitTokenResult { token: None });
        };
        let Some(url) = normalize_url(url) else {
            return Err(bad_request(format!("{url:?} is not an HTTPS URL")));
        };
        let providers = self.store.providers()?;
        let found = providers
            .iter()
            .find_map(|p| provider_path(&p.web_url, &url).map(|path| (p, path)));
        let Some((provider, (owner, repository))) = found else {
            return Ok(GitTokenResult { token: None });
        };
        let credentials = self.store.preset_credentials(preset)?;
        let Some(credential) = select_credential(&credentials, provider.id, &owner, &repository)
        else {
            return Ok(GitTokenResult { token: None });
        };
        tracing::debug!(codespace = %row.id, credential = %credential.name, %owner, %repository, "gave a Git token");
        Ok(GitTokenResult {
            token: Some(GitToken {
                username: credential
                    .login
                    .clone()
                    .unwrap_or_else(|| TOKEN_USER.to_owned()),
                password: self.token(credential)?,
                expires_at: credential.expires_at,
            }),
        })
    }

    pub async fn search_repositories(
        self: &Arc<Self>,
        provider: &str,
        query: &str,
    ) -> Result<RepositorySearch, OracleError> {
        let provider = self.provider_row(provider)?;
        let query = query.trim();
        if query.len() > 200 || query.chars().any(char::is_control) {
            return Err(bad_request("the query is one line of up to 200 bytes"));
        }
        let credentials: Vec<CredentialRow> = self
            .store
            .credentials()?
            .into_iter()
            .filter(|c| c.provider == provider.id)
            .collect();
        let mut clients = Vec::new();
        for credential in &credentials {
            clients.push((
                Some(credential.name.clone()),
                self.github(&provider, Some(self.token(credential)?)),
            ));
        }
        if clients.is_empty() {
            clients.push((None, self.github(&provider, None)));
        }
        let lower = query.to_lowercase();
        let mut found: Vec<RemoteRepository> = Vec::new();
        let mut errors = Vec::new();
        let mut more = false;
        let mut add =
            |page: Page<loom_github::RemoteRepository>, filter: bool, more_flag: &mut bool| {
                *more_flag |= page.more;
                for repository in page.items {
                    let full = format!("{}/{}", repository.owner, repository.name).to_lowercase();
                    if filter && !full.contains(&lower) {
                        continue;
                    }
                    if !found
                        .iter()
                        .any(|r| format!("{}/{}", r.owner, r.name).to_lowercase() == full)
                    {
                        found.push(remote(repository));
                    }
                }
            };
        for (name, github) in &clients {
            let label = name.as_deref().unwrap_or("no credential");
            if name.is_some() {
                match github.user_repositories(100).await {
                    Ok(page) => add(page, !query.is_empty(), &mut more),
                    Err(err) => errors.push(format!("{label}: {err}")),
                }
            }
            if !query.is_empty() {
                let search = if query.contains(':') {
                    query.to_owned()
                } else {
                    format!("{query} in:name")
                };
                match github.search_repositories(&search, SEARCH_LIMIT).await {
                    Ok(page) => add(page, false, &mut more),
                    Err(err) => errors.push(format!("{label}: {err}")),
                }
            }
        }
        if found.is_empty() && !errors.is_empty() && errors.len() >= clients.len() {
            return Err(unavailable(errors.join("; ")));
        }
        more |= found.len() > MAX_FOUND;
        found.truncate(MAX_FOUND);
        Ok(RepositorySearch {
            repositories: found,
            more,
            errors,
        })
    }

    pub async fn list_branches(
        self: &Arc<Self>,
        provider: &str,
        owner: &str,
        repository: &str,
    ) -> Result<Branches, OracleError> {
        let provider = self.provider_row(provider)?;
        if repository_path(&format!("{owner}/{repository}")).is_none() {
            return Err(bad_request(format!(
                "{owner}/{repository} is not a repository of GitHub"
            )));
        }
        let mut credentials: Vec<CredentialRow> = self
            .store
            .credentials()?
            .into_iter()
            .filter(|c| c.provider == provider.id)
            .collect();
        credentials.sort_by_key(|c| {
            let matches =
                select_credential(std::slice::from_ref(c), provider.id, owner, repository)
                    .is_some();
            (!matches, rank(c))
        });
        let mut tokens = Vec::new();
        for credential in &credentials {
            tokens.push(Some(self.token(credential)?));
        }
        tokens.push(None);
        let mut last = None;
        for token in tokens {
            let github = self.github(&provider, token);
            let result = async {
                let found = github.repository(owner, repository).await?;
                let branches = github.branches(owner, repository, BRANCH_LIMIT).await?;
                Ok::<_, GitHubError>(Branches {
                    branches: branches.items,
                    default_branch: found.default_branch,
                    more: branches.more,
                })
            }
            .await;
            match result {
                Ok(branches) => return Ok(branches),
                Err(err) if matches!(err.status(), Some(401 | 403 | 404)) => last = Some(err),
                Err(err) => return Err(github_error(err)),
            }
        }
        Err(last.map_or_else(
            || not_found(format!("cannot find {owner}/{repository}")),
            github_error,
        ))
    }
}

fn main_credential<'a>(
    provider: &ProviderRow,
    repositories: &[Repository],
    credentials: &'a [CredentialRow],
) -> Option<&'a CredentialRow> {
    let first = repositories
        .iter()
        .filter(|r| r.provider.as_deref() == Some(provider.name.as_str()))
        .find_map(|r| provider_path(&provider.web_url, &r.url));
    match &first {
        Some((owner, repository)) => select_credential(credentials, provider.id, owner, repository),
        None => None,
    }
    .or_else(|| widest(credentials, provider.id))
}

fn identity(
    provider: &ProviderRow,
    credential: Option<&CredentialRow>,
) -> (Option<String>, Option<String>) {
    let name = provider
        .git_name
        .clone()
        .or_else(|| credential.and_then(|c| c.login.clone()));
    let email = provider.git_email.clone().or_else(|| {
        let credential = credential?;
        let host = url_host(&provider.web_url);
        let login = credential.login.as_deref()?;
        let id = credential.user_id?;
        host.eq_ignore_ascii_case(GITHUB_HOST)
            .then(|| format!("{id}+{login}@users.noreply.{GITHUB_HOST}"))
    });
    (name, email)
}

pub fn resolve_repository(
    input: &Repository,
    providers: &[ProviderRow],
) -> Result<Repository, OracleError> {
    let text = input.url.trim();
    let short = text
        .split_once(':')
        .filter(|(name, _)| !text.contains("://") && valid_name(name));
    let (provider, url) = if let Some((name, path)) = short {
        let provider = providers
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| not_found(format!("no Git provider has the name {name}")))?;
        let (owner, repository) = repository_path(path)
            .ok_or_else(|| bad_request(format!("{path:?} is not an owner and a repository")))?;
        (
            Some(provider),
            format!("{}/{owner}/{repository}.git", provider.web_url),
        )
    } else {
        let url = normalize_url(text).ok_or_else(|| {
            bad_request(format!(
                "{text:?} is not an HTTPS URL of a repository or provider:owner/repository"
            ))
        })?;
        let provider = match &input.provider {
            Some(name) => {
                let provider = providers
                    .iter()
                    .find(|p| &p.name == name)
                    .ok_or_else(|| not_found(format!("no Git provider has the name {name}")))?;
                if provider_path(&provider.web_url, &url).is_none() {
                    return Err(bad_request(format!(
                        "{url} is not a repository of the Git provider {name}"
                    )));
                }
                Some(provider)
            }
            None => providers
                .iter()
                .find(|p| provider_path(&p.web_url, &url).is_some()),
        };
        match provider.and_then(|p| Some((p, provider_path(&p.web_url, &url)?))) {
            Some((provider, (owner, repository))) => (
                Some(provider),
                format!("{}/{owner}/{repository}.git", provider.web_url),
            ),
            None => (None, url),
        }
    };
    let directory = match &input.directory {
        Some(directory) if valid_directory(directory) => directory.clone(),
        Some(directory) => {
            return Err(bad_request(format!(
                "{directory:?} is not a directory name: use letters, digits, '-', '_', and '.'"
            )));
        }
        None => repository_directory(&url).ok_or_else(|| {
            bad_request(format!("cannot find a directory name in {url:?}; set one"))
        })?,
    };
    if let Some(branch) = &input.base_branch
        && !super::valid_branch(branch)
    {
        return Err(bad_request(format!("{branch:?} is not a branch name")));
    }
    Ok(Repository {
        url,
        provider: provider.map(|p| p.name.clone()),
        directory: Some(directory),
        base_branch: input.base_branch.clone(),
    })
}

pub(super) fn with_base_branches(
    repositories: &[Repository],
    all: Option<&str>,
    branches: &BTreeMap<String, String>,
) -> Result<Vec<Repository>, OracleError> {
    for (directory, branch) in branches {
        if !repositories
            .iter()
            .any(|r| r.directory.as_deref() == Some(directory.as_str()))
        {
            return Err(bad_request(format!(
                "the workspace has no repository in the directory {directory}"
            )));
        }
        if !super::valid_branch(branch) {
            return Err(bad_request(format!("{branch:?} is not a branch name")));
        }
    }
    Ok(repositories
        .iter()
        .map(|repository| Repository {
            base_branch: repository
                .directory
                .as_ref()
                .and_then(|directory| branches.get(directory))
                .map(String::as_str)
                .or(all)
                .map(str::to_owned)
                .or_else(|| repository.base_branch.clone()),
            ..repository.clone()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: u64, name: &str, web_url: &str) -> ProviderRow {
        ProviderRow {
            id,
            name: name.into(),
            web_url: web_url.into(),
            api_url: default_api_url(web_url),
            git_name: None,
            git_email: None,
            route: ProviderRoute::Direct,
            created_at: 0,
        }
    }

    fn credential(
        id: u64,
        provider: u64,
        owner: Option<&str>,
        repository: Option<&str>,
    ) -> CredentialRow {
        CredentialRow {
            id,
            name: format!("c{id}"),
            provider,
            provider_name: "p".into(),
            owner: owner.map(str::to_owned),
            repository: repository.map(str::to_owned),
            login: None,
            user_id: None,
            expires_at: None,
            checked_at: None,
            check_error: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn input(url: &str) -> Repository {
        Repository {
            url: url.into(),
            provider: None,
            directory: None,
            base_branch: None,
        }
    }

    #[test]
    fn urls_are_normalized_and_checked() {
        assert_eq!(
            normalize_url(" https://ghe.example.com/ ").as_deref(),
            Some("https://ghe.example.com")
        );
        for bad in [
            "ftp://x",
            "https://",
            "https://user:pw@x/a",
            "https://x/a?b",
            "https://x/../y",
            "https://x//y",
        ] {
            assert_eq!(normalize_url(bad), None, "{bad}");
        }
        assert_eq!(
            provider_path("https://github.com", "https://GitHub.com/Acme/Server.git"),
            Some(("Acme".into(), "Server".into()))
        );
        assert_eq!(
            provider_path("https://github.com", "https://github.community/a/b"),
            None
        );
    }

    #[test]
    fn repositories_belong_to_their_provider() {
        let providers = [
            provider(1, "github", "https://github.com"),
            provider(2, "ghe", "https://ghe.example.com"),
        ];
        let resolved =
            resolve_repository(&input("https://github.com/acme/server"), &providers).unwrap();
        assert_eq!(resolved.url, "https://github.com/acme/server.git");
        assert_eq!(resolved.provider.as_deref(), Some("github"));
        assert_eq!(resolved.directory.as_deref(), Some("server"));
        let short = resolve_repository(&input("ghe:acme/web.app"), &providers).unwrap();
        assert_eq!(short.url, "https://ghe.example.com/acme/web.app.git");
        assert_eq!(short.directory.as_deref(), Some("web.app"));
        let public = resolve_repository(&input("https://gitlab.com/x/y.git"), &providers).unwrap();
        assert_eq!(public.provider, None);
        assert_eq!(public.url, "https://gitlab.com/x/y.git");
        let wrong = Repository {
            provider: Some("ghe".into()),
            ..input("https://github.com/acme/server")
        };
        assert!(resolve_repository(&wrong, &providers).is_err());
        for bad in ["nope:acme/x", "git@github.com:acme/server.git", "ghe:acme"] {
            assert!(
                resolve_repository(&input(bad), &providers).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_smallest_matching_scope_wins() {
        let credentials = [
            credential(1, 1, None, None),
            credential(2, 1, Some("acme"), None),
            credential(3, 1, Some("acme"), Some("server")),
            credential(4, 2, None, None),
            credential(5, 1, None, None),
        ];
        let pick = |owner: &str, repository: &str| {
            select_credential(&credentials, 1, owner, repository).map(|c| c.id)
        };
        assert_eq!(pick("acme", "server"), Some(3));
        assert_eq!(pick("ACME", "Server"), Some(3));
        assert_eq!(pick("acme", "client"), Some(2));
        assert_eq!(pick("other", "x"), Some(1));
        assert_eq!(select_credential(&credentials, 3, "a", "b"), None);
        assert_eq!(widest(&credentials, 1).map(|c| c.id), Some(1));
        assert_eq!(widest(&credentials[1..3], 1).map(|c| c.id), Some(2));
    }

    #[test]
    fn the_identity_comes_from_the_provider_or_the_login() {
        let github = provider(1, "github", "https://github.com");
        let mut known = credential(1, 1, None, None);
        known.login = Some("octocat".into());
        known.user_id = Some(583231);
        assert_eq!(
            identity(&github, Some(&known)),
            (
                Some("octocat".into()),
                Some("583231+octocat@users.noreply.github.com".into())
            )
        );
        let ghe = ProviderRow {
            git_name: Some("Me".into()),
            ..provider(1, "ghe", "https://ghe.example.com")
        };
        assert_eq!(identity(&ghe, Some(&known)), (Some("Me".into()), None));
        assert_eq!(identity(&github, None), (None, None));
    }

    #[test]
    fn presets_are_agent_files_with_credentials() {
        let config = parse_preset("work", "git:\n  credentials: [ghe-work]\n").unwrap();
        assert_eq!(config.git.credentials, vec!["ghe-work".to_owned()]);
        assert!(parse_preset("work", "git:\n  tokens: []\n").is_err());
        assert!(parse_preset("work", &"#".repeat(MAX_PRESET + 1)).is_err());
    }
}
