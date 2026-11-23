use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use loom_llm::{
    AccountInfo, Auth, CredentialLock, CredentialSource, CredentialStore, Credentials, HttpClient,
    LlmError, LoginInput, LoginMethod, LoginPrompt, ProviderId, ProviderSet, known_providers,
    limit_to_accounts,
};
use loom_oracle_proto::{
    AccountKind, ErrorCode, Event, ModelAccount, ModelCredentials, ModelEntry, ModelLogin,
    ModelLoginInput, ModelLoginMethod, ModelLoginPrompt, Models, OracleError, Secret, valid_name,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Oracle, bad_request, not_found};
use crate::store::{ModelAccountRow, NewModelAccount, PresetRow, Store, now_unix};

const LOGIN_TTL: Duration = Duration::from_secs(20 * 60);
const LEND_MARGIN_SECS: u64 = 30 * 60;
const REFETCH_BEFORE_SECS: u64 = 15 * 60;
const MIN_VALID_SECS: u64 = 60;
const MAX_API_KEY: usize = 4096;

pub(super) struct ModelState {
    http: HttpClient,
    auth_urls: Mutex<HashMap<String, String>>,
    logins: Mutex<HashMap<String, PendingLogin>>,
    locks: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
}

struct PendingLogin {
    account: String,
    provider: String,
    auth: Auth,
    source: Arc<MemorySource>,
    login: String,
    created: Instant,
    cancel: CancellationToken,
}

impl ModelState {
    pub(super) fn new() -> Self {
        Self {
            http: loom_llm::http_client(),
            auth_urls: Mutex::new(HashMap::new()),
            logins: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn auth(&self, store: CredentialStore) -> Auth {
        let urls = self
            .auth_urls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        urls.into_iter().fold(
            Auth::new(store, self.http.clone()),
            |auth, (provider, url)| auth.with_auth_base_url(provider, url),
        )
    }

    fn lock_for(&self, account: u64) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account)
            .or_default()
            .clone()
    }

    fn lock_logins(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingLogin>> {
        self.logins.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct MemorySource {
    credentials: Mutex<Option<Credentials>>,
}

impl MemorySource {
    fn get(&self) -> Credentials {
        self.credentials
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }

    fn set(&self, credentials: Credentials) {
        *self.credentials.lock().unwrap_or_else(|e| e.into_inner()) = Some(credentials);
    }
}

struct MemoryLock(Arc<MemorySource>);

#[async_trait]
impl CredentialLock for MemoryLock {
    async fn load(&self) -> loom_llm::Result<Credentials> {
        Ok(self.0.get())
    }

    async fn save(&self, credentials: Credentials) -> loom_llm::Result<()> {
        self.0.set(credentials);
        Ok(())
    }
}

struct MemoryHandle(Arc<MemorySource>);

#[async_trait]
impl CredentialSource for MemoryHandle {
    fn location(&self) -> PathBuf {
        PathBuf::from("oracle sign-in")
    }

    async fn load(&self) -> loom_llm::Result<Credentials> {
        Ok(self.0.get())
    }

    async fn lock(&self) -> loom_llm::Result<Box<dyn CredentialLock>> {
        Ok(Box::new(MemoryLock(self.0.clone())))
    }
}

struct AccountSource {
    store: Arc<Store>,
    id: u64,
    lock: Arc<tokio::sync::Mutex<()>>,
}

fn store_error(err: impl std::fmt::Display) -> LlmError {
    LlmError::Unsupported(format!("cannot read the model account: {err}"))
}

fn read_account(store: &Store, id: u64) -> loom_llm::Result<Credentials> {
    let bytes = store.model_account_credentials(id).map_err(store_error)?;
    if bytes.is_empty() {
        return Ok(Credentials::default());
    }
    serde_json::from_slice(&bytes).map_err(store_error)
}

struct AccountLock {
    store: Arc<Store>,
    id: u64,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

#[async_trait]
impl CredentialLock for AccountLock {
    async fn load(&self) -> loom_llm::Result<Credentials> {
        read_account(&self.store, self.id)
    }

    async fn save(&self, credentials: Credentials) -> loom_llm::Result<()> {
        let bytes = serde_json::to_vec(&credentials).map_err(store_error)?;
        self.store
            .set_model_account_credentials(self.id, &bytes)
            .map_err(store_error)
    }
}

#[async_trait]
impl CredentialSource for AccountSource {
    fn location(&self) -> PathBuf {
        PathBuf::from(format!("oracle model account {}", self.id))
    }

    async fn load(&self) -> loom_llm::Result<Credentials> {
        read_account(&self.store, self.id)
    }

    async fn lock(&self) -> loom_llm::Result<Box<dyn CredentialLock>> {
        let guard = self.lock.clone().lock_owned().await;
        Ok(Box::new(AccountLock {
            store: self.store.clone(),
            id: self.id,
            _guard: guard,
        }))
    }
}

fn llm_error(err: LlmError) -> OracleError {
    match err {
        LlmError::UnknownProvider(provider) => {
            bad_request(format!("{provider} is not a model provider of Loom"))
        }
        LlmError::Unsupported(message) => bad_request(message),
        LlmError::Login(message) => OracleError::new(ErrorCode::Forbidden, message),
        other => OracleError::new(ErrorCode::Unavailable, other.to_string()),
    }
}

fn prompt(prompt: LoginPrompt) -> ModelLoginPrompt {
    match prompt {
        LoginPrompt::OpenUrl { url, redirect_uri } => {
            ModelLoginPrompt::OpenUrl { url, redirect_uri }
        }
        LoginPrompt::DeviceCode {
            user_code,
            verification_uri,
            expires_in_secs,
        } => ModelLoginPrompt::DeviceCode {
            user_code,
            verification_uri,
            expires_in_secs,
        },
    }
}

fn is_builtin(provider: &str) -> bool {
    known_providers().iter().any(|id| id.as_str() == provider)
}

fn valid_provider(provider: &str) -> bool {
    !provider.is_empty()
        && provider.len() <= 64
        && provider
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

impl Oracle {
    pub fn set_model_auth_base_url(&self, provider: &str, url: &str) {
        self.models
            .auth_urls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(provider.to_owned(), url.to_owned());
    }

    fn account_auth(&self, id: u64) -> Auth {
        let source = AccountSource {
            store: self.store.clone(),
            id,
            lock: self.models.lock_for(id),
        };
        self.models
            .auth(CredentialStore::from_source(Arc::new(source)))
    }

    pub(super) fn model_account(&self, row: &ModelAccountRow) -> Result<ModelAccount, OracleError> {
        Ok(ModelAccount {
            name: row.name.clone(),
            provider: row.provider.clone(),
            kind: row.kind,
            email: row.email.clone(),
            organization: row.organization.clone(),
            account_id: row.account_id.clone(),
            error: row.error.clone(),
            presets: self.store.presets_with_account(row.id)?,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    pub fn model_accounts(&self) -> Result<Vec<ModelAccount>, OracleError> {
        self.store
            .model_accounts()?
            .iter()
            .map(|row| self.model_account(row))
            .collect()
    }

    fn account_changed(&self, row: &ModelAccountRow) {
        if let Ok(account) = self.model_account(row) {
            if !account.presets.is_empty() {
                self.emit(Event::ConfigChanged {
                    presets: account.presets.clone(),
                });
            }
            self.emit(Event::ModelAccountChanged { account });
        }
    }

    fn check_account_name(&self, name: &str, provider: &str) -> Result<(), OracleError> {
        if !valid_name(name) {
            return Err(bad_request(format!(
                "{name:?} is not an account name: use 1 to 40 lowercase letters, digits, and '-', and start with a letter"
            )));
        }
        if !valid_provider(provider) {
            return Err(bad_request(format!(
                "{provider:?} is not a model provider ID"
            )));
        }
        if let Some(row) = self.store.model_account_by_name(name)?
            && row.provider != provider
        {
            return Err(bad_request(format!(
                "the model account {name} is for {}; use a different name for {provider}",
                row.provider
            )));
        }
        Ok(())
    }

    pub async fn start_model_login(
        self: &Arc<Self>,
        account: &str,
        provider: &str,
        method: ModelLoginMethod,
        by: Option<u64>,
    ) -> Result<ModelLogin, OracleError> {
        self.check_account_name(account, provider)?;
        if !is_builtin(provider) {
            return Err(bad_request(format!(
                "{provider} has no sign-in; give an API key"
            )));
        }
        let source = Arc::new(MemorySource::default());
        let auth = self
            .models
            .auth(CredentialStore::from_source(Arc::new(MemoryHandle(
                source.clone(),
            ))));
        let method = match method {
            ModelLoginMethod::Browser { redirect_uri } => LoginMethod::Browser {
                redirect_uri,
                account: None,
            },
            ModelLoginMethod::DeviceCode => LoginMethod::DeviceCode,
        };
        let device_code = matches!(method, LoginMethod::DeviceCode);
        let started = auth
            .start_login(&ProviderId::new(provider), method)
            .await
            .map_err(llm_error)?;
        let id = uuid_like();
        let cancel = CancellationToken::new();
        {
            let mut logins = self.models.lock_logins();
            logins.retain(|_, login| login.created.elapsed() < LOGIN_TTL);
            logins.insert(
                id.clone(),
                PendingLogin {
                    account: account.to_owned(),
                    provider: provider.to_owned(),
                    auth: auth.clone(),
                    source,
                    login: started.login_id.clone(),
                    created: Instant::now(),
                    cancel: cancel.clone(),
                },
            );
        }
        self.audit(
            by,
            "model_login_started",
            &format!("model account {account} of {provider}"),
        );
        if device_code {
            let oracle = self.clone();
            let login = id.clone();
            tokio::spawn(async move {
                let result = auth.wait_login(&started.login_id, &cancel).await;
                if cancel.is_cancelled() {
                    return;
                }
                let finished = oracle.finish_login(&login, result, by).await;
                let (account, error) = match finished {
                    Ok(account) => (Some(account), None),
                    Err(err) => (None, Some(err.message)),
                };
                oracle.emit(Event::ModelLoginFinished {
                    login,
                    account,
                    error,
                });
            });
        }
        Ok(ModelLogin {
            login: id,
            account: account.to_owned(),
            provider: provider.to_owned(),
            prompt: prompt(started.prompt),
        })
    }

    pub async fn complete_model_login(
        &self,
        login: &str,
        input: ModelLoginInput,
        by: Option<u64>,
    ) -> Result<ModelAccount, OracleError> {
        let (auth, inner) = {
            let logins = self.models.lock_logins();
            let pending = logins
                .get(login)
                .ok_or_else(|| not_found("no sign-in has this ID, or it expired"))?;
            (pending.auth.clone(), pending.login.clone())
        };
        let input = match input {
            ModelLoginInput::Redirect { url } => LoginInput::Redirect { url },
            ModelLoginInput::Code { code } => LoginInput::Code { code },
        };
        let result = auth.complete_login(&inner, input).await;
        let account = self.finish_login(login, result, by).await?;
        self.emit(Event::ModelLoginFinished {
            login: login.to_owned(),
            account: Some(account.clone()),
            error: None,
        });
        Ok(account)
    }

    pub fn cancel_model_login(&self, login: &str) -> Result<(), OracleError> {
        let pending = self
            .models
            .lock_logins()
            .remove(login)
            .ok_or_else(|| not_found("no sign-in has this ID, or it expired"))?;
        pending.cancel.cancel();
        pending.auth.cancel_login(&pending.login);
        Ok(())
    }

    async fn finish_login(
        &self,
        login: &str,
        result: loom_llm::Result<AccountInfo>,
        by: Option<u64>,
    ) -> Result<ModelAccount, OracleError> {
        let pending = self
            .models
            .lock_logins()
            .remove(login)
            .ok_or_else(|| not_found("no sign-in has this ID, or it expired"))?;
        let info = result.map_err(llm_error)?;
        let provider = ProviderId::new(&pending.provider);
        let identity = pending
            .auth
            .account_identity(&provider)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let credentials = pending.source.get();
        let section = credentials
            .section(&pending.provider)
            .cloned()
            .ok_or_else(|| bad_request("the sign-in gave no credential"))?;
        let mut stored = Credentials::default();
        stored.set_section(&pending.provider, Some(section));
        let bytes = loom_crypto::Zeroizing::new(
            serde_json::to_vec(&stored).map_err(|e| bad_request(e.to_string()))?,
        );
        let email = identity.email.or(info.label);
        let account_id = info.account_id.or(identity.account_uuid);
        let row = self.store.put_model_account(&NewModelAccount {
            name: &pending.account,
            provider: &pending.provider,
            kind: AccountKind::SignIn,
            credentials: &bytes,
            email: email.as_deref(),
            organization: identity.organization_uuid.as_deref(),
            account_id: account_id.as_deref(),
        })?;
        self.audit(
            by,
            "model_account_signed_in",
            &format!("model account {} of {}", row.name, row.provider),
        );
        tracing::info!(account = %row.name, provider = %row.provider, "signed in a model account");
        self.account_changed(&row);
        self.model_account(&row)
    }

    pub async fn set_model_api_key(
        &self,
        account: &str,
        provider: &str,
        api_key: &Secret,
        by: Option<u64>,
    ) -> Result<ModelAccount, OracleError> {
        self.check_account_name(account, provider)?;
        let key = api_key.expose().trim();
        if key.is_empty() || key.len() > MAX_API_KEY || key.chars().any(char::is_control) {
            return Err(bad_request(format!(
                "an API key is one line of 1 to {MAX_API_KEY} bytes"
            )));
        }
        let mut stored = Credentials::default();
        if is_builtin(provider) {
            let source = Arc::new(MemorySource::default());
            let auth = self
                .models
                .auth(CredentialStore::from_source(Arc::new(MemoryHandle(
                    source.clone(),
                ))));
            auth.set_api_key(&ProviderId::new(provider), Some(key.to_owned()))
                .await
                .map_err(llm_error)?;
            stored = source.get();
        } else {
            stored.set_section(provider, Some(json!({"api_key": key})));
        }
        let bytes = loom_crypto::Zeroizing::new(
            serde_json::to_vec(&stored).map_err(|e| bad_request(e.to_string()))?,
        );
        let row = self.store.put_model_account(&NewModelAccount {
            name: account,
            provider,
            kind: AccountKind::ApiKey,
            credentials: &bytes,
            email: None,
            organization: None,
            account_id: None,
        })?;
        self.audit(
            by,
            "model_account_set",
            &format!("model account {account} of {provider} with an API key"),
        );
        self.account_changed(&row);
        self.model_account(&row)
    }

    pub fn delete_model_account(&self, name: &str, by: Option<u64>) -> Result<(), OracleError> {
        if !self.store.delete_model_account(name)? {
            return Err(not_found(format!("no model account has the name {name}")));
        }
        self.audit(
            by,
            "model_account_deleted",
            &format!("model account {name}"),
        );
        self.emit(Event::ModelAccountRemoved {
            name: name.to_owned(),
        });
        Ok(())
    }

    async fn lend_account(&self, id: u64, provider: &str) -> Result<(Value, Option<u64>), String> {
        let lent = if is_builtin(provider) {
            self.account_auth(id)
                .lend(&ProviderId::new(provider), LEND_MARGIN_SECS)
                .await
                .map_err(|err| err.to_string())?
                .map(|lent| (lent.section, lent.expires_at))
        } else {
            read_account(&self.store, id)
                .map_err(|err| err.to_string())?
                .section(provider)
                .cloned()
                .map(|section| (section, None))
        };
        lent.ok_or_else(|| "the account has no credential; sign in again".to_owned())
    }

    async fn preset_credentials(&self, preset: &PresetRow) -> ModelCredentials {
        let mut credentials = ModelCredentials {
            sections: BTreeMap::new(),
            accounts: BTreeMap::new(),
            valid_secs: None,
            errors: Vec::new(),
        };
        let now = now_unix();
        let mut earliest: Option<u64> = None;
        for (provider, id, name) in &preset.accounts {
            credentials.accounts.insert(provider.clone(), name.clone());
            let result = self.lend_account(*id, provider).await;
            let error = result.as_ref().err().cloned();
            if let Ok(true) = self.store.set_model_account_error(*id, error.as_deref())
                && let Ok(Some(row)) = self.store.model_account(*id)
                && let Ok(account) = self.model_account(&row)
            {
                self.emit(Event::ModelAccountChanged { account });
            }
            match result {
                Ok((section, expires_at)) => {
                    credentials.sections.insert(provider.clone(), section);
                    if let Some(expires_at) = expires_at {
                        earliest = Some(earliest.map_or(expires_at, |e| e.min(expires_at)));
                    }
                }
                Err(error) => {
                    tracing::warn!(account = %name, %provider, %error, "cannot lend a model credential");
                    credentials
                        .errors
                        .push(format!("model account {name} ({provider}): {error}"));
                }
            }
        }
        credentials.valid_secs = earliest.map(|expires_at| {
            expires_at
                .saturating_sub(now)
                .saturating_sub(REFETCH_BEFORE_SECS)
                .max(MIN_VALID_SECS)
        });
        credentials
    }

    pub async fn model_credentials(&self, peer: u64) -> Result<ModelCredentials, OracleError> {
        let row = self.codespace_of_peer(peer)?;
        if !row.state.is_open() {
            return Err(OracleError::new(
                ErrorCode::Conflict,
                format!("the codespace is {}", row.state),
            ));
        }
        let Some(preset) = row
            .preset
            .map(|id| self.store.preset(id))
            .transpose()?
            .flatten()
        else {
            return Ok(ModelCredentials {
                sections: BTreeMap::new(),
                accounts: BTreeMap::new(),
                valid_secs: None,
                errors: Vec::new(),
            });
        };
        Ok(self.preset_credentials(&preset).await)
    }

    pub async fn list_models(&self, preset: &str) -> Result<Models, OracleError> {
        let row = self.preset_by_name(preset)?;
        let config = super::git::parse_preset(&row.name, &row.yaml)?;
        let mut llm = config.llm.clone();
        limit_to_accounts(&mut llm);
        let lent = self.preset_credentials(&row).await;
        let source = Arc::new(MemorySource::default());
        let mut credentials = Credentials::default();
        for (provider, section) in &lent.sections {
            credentials.set_section(provider, Some(section.clone()));
        }
        source.set(credentials);
        let auth = self
            .models
            .auth(CredentialStore::from_source(Arc::new(MemoryHandle(source))));
        let providers = ProviderSet::from_config(&llm, &auth).map_err(llm_error)?;
        providers.activate(&auth);
        let mut models = Models {
            models: Vec::new(),
            default_model: llm.default_model.clone(),
            errors: lent.errors,
        };
        for id in providers.ids() {
            let account = lent.accounts.get(id.as_str()).cloned();
            if account.is_none() && is_builtin(id.as_str()) {
                continue;
            }
            let listed = match providers.get(&id) {
                Ok(provider) => provider.list_models().await,
                Err(err) => Err(err),
            };
            match listed {
                Ok(list) => models
                    .models
                    .extend(list.into_iter().map(|model| ModelEntry {
                        id: format!("{id}/{}", model.id),
                        provider: id.to_string(),
                        display_name:
                            (model.display_name != model.id).then_some(model.display_name),
                        account: account.clone(),
                    })),
                Err(err) => models.errors.push(format!("{id}: {err}")),
            }
        }
        Ok(models)
    }
}

fn uuid_like() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_checked() {
        assert!(valid_provider("anthropic"));
        assert!(valid_provider("my_llm-2"));
        assert!(!valid_provider("Anthropic"));
        assert!(!valid_provider("a/b"));
        assert!(!valid_provider(""));
        assert!(is_builtin("openai-codex"));
        assert!(!is_builtin("my-llm"));
    }

    #[tokio::test]
    async fn an_account_source_keeps_credentials_in_the_store() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        store.unlock(&crate::seal::MasterKey::generate()).unwrap();
        let row = store
            .put_model_account(&NewModelAccount {
                name: "work",
                provider: "anthropic",
                kind: AccountKind::SignIn,
                credentials: b"",
                email: None,
                organization: None,
                account_id: None,
            })
            .unwrap();
        let source = CredentialStore::from_source(Arc::new(AccountSource {
            store: store.clone(),
            id: row.id,
            lock: Arc::default(),
        }));
        assert!(source.load().await.unwrap().section("anthropic").is_none());
        source
            .update(|credentials| {
                credentials.set_section("anthropic", Some(json!({"access_token": "a"})))
            })
            .await
            .unwrap();
        assert_eq!(
            read_account(&store, row.id).unwrap().section("anthropic"),
            Some(&json!({"access_token": "a"}))
        );
    }
}
