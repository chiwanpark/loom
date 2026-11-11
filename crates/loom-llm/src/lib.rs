mod auth;
mod error;
mod kind;
mod oauth;
mod provider;
mod providers;
mod registry;
mod responses;
mod retry;
mod sse;
mod store;
mod types;
mod usage;
mod web_search;

pub use auth::{
    AccountInfo, AccountStatus, Auth, LoginInput, LoginMethod, LoginMethodInfo, LoginPrompt,
    LoginStarted, ProviderStatus, RedirectSpec,
};
pub use error::{LlmError, Result};
pub use provider::{ChunkStream, LlmProvider, ProviderId, collect};
pub use registry::{ModelRef, ProviderSet, http_client, known_providers, validate_config};
pub use reqwest::Client as HttpClient;
pub use retry::RetryPolicy;
pub use store::{CREDENTIALS_FILE, CredentialStore, Credentials, StoreGuard, now_unix};
pub use types::*;
pub use usage::{UsageLimit, UsageLimits};
pub use web_search::{
    DEFAULT_SEARCH_ENDPOINT, Freshness, WebSearch, WebSearchQuery, WebSearchResult,
    WebSearchSettings, WebSource,
};
