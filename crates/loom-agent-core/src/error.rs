use thiserror::Error;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("the session is busy with another turn")]
    Busy,
    #[error("no model selected; set llm.default_model or choose a model for the session")]
    NoModel,
    #[error("unknown session {0}")]
    UnknownSession(String),
    #[error("no pending approval for tool call {0}")]
    UnknownApproval(String),
    #[error("prompt must not be empty")]
    EmptyPrompt,
    #[error("there is nothing to compact in this session yet")]
    NothingToCompact,
    #[error(transparent)]
    Llm(#[from] loom_llm::LlmError),
    #[error(transparent)]
    Tool(#[from] loom_tools::ToolError),
}
