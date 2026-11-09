use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    #[error("path {0} is outside the workspace")]
    OutsideWorkspace(PathBuf),

    #[error("file not found: {0}")]
    NotFound(PathBuf),

    #[error("{path} was modified since it was last read; read it again before changing it")]
    StaleFile { path: PathBuf },

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    #[error("{0}")]
    Failed(String),

    #[error("the tool call was cancelled")]
    Cancelled,
}

impl ToolError {
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

pub type Result<T, E = ToolError> = std::result::Result<T, E>;
