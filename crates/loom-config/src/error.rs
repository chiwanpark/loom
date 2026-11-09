use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("HOME is not set to an absolute path")]
    NoHome,

    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{origin}: {message}")]
    Parse { origin: String, message: String },

    #[error("{origin}: {field}: {message}")]
    Invalid {
        origin: String,
        field: String,
        message: String,
    },
}

impl ConfigError {
    pub(crate) fn invalid(
        origin: impl Into<String>,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Invalid {
            origin: origin.into(),
            field: field.into(),
            message: message.into(),
        }
    }
}
