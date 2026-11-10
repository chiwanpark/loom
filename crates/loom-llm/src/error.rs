use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

pub type Result<T, E = LlmError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("provider {0} is unknown or disabled")]
    UnknownProvider(String),
    #[error("{field}: {message}")]
    Config { field: String, message: String },
    #[error("{0}")]
    Unsupported(String),
    #[error("invalid model {0:?}: use <provider>/<model>")]
    InvalidModel(String),
    #[error("not signed in to {provider}: {message}")]
    NotAuthenticated { provider: String, message: String },
    #[error("login failed: {0}")]
    Login(String),
    #[error("login cancelled")]
    Cancelled,
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{provider} API error ({status}): {message}")]
    Api {
        provider: String,
        status: u16,
        code: Option<String>,
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("{provider} stream error: {message}")]
    Stream {
        provider: String,
        code: Option<String>,
        message: String,
    },
    #[error("invalid response from {provider}: {message}")]
    Protocol { provider: String, message: String },
    #[error("credential store {path}: {message}")]
    Store { path: PathBuf, message: String },
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LlmError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } | Self::Stream { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn protocol(provider: &str, message: impl Into<String>) -> Self {
        Self::Protocol {
            provider: provider.to_owned(),
            message: message.into(),
        }
    }

    pub(crate) fn login(message: impl Into<String>) -> Self {
        Self::Login(message.into())
    }
}
