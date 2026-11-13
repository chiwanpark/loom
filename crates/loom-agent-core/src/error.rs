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
    #[error(
        "the notes would be {size} bytes, more than the {max}-byte limit (tools.notes.max_bytes)"
    )]
    NotesTooLarge { size: usize, max: u64 },
    #[error("the notes changed since they were read")]
    NotesChanged,
    #[error("the task list would have more than {max} tasks (tools.tasks.max_items)")]
    TooManyTasks { max: u32 },
    #[error("there is no task #{id}")]
    NoSuchTask { id: u32 },
    #[error("a task's text must not be empty")]
    EmptyTask,
    #[error(transparent)]
    Llm(#[from] loom_llm::LlmError),
    #[error(transparent)]
    Tool(#[from] loom_tools::ToolError),
}
