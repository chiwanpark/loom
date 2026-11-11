mod edit;
mod error;
mod fsutil;
pub mod pty;
mod read;
mod registry;
pub mod sandbox;
mod shell;
mod tool;
mod truncate;
pub mod vtext;
mod workspace;
mod write;

pub use edit::{EditTool, Replacement, apply_edits};
pub use error::{Result, ToolError};
pub use fsutil::write_file;
pub use read::ReadTool;
pub use registry::{BuiltinConfig, ToolRegistry};
pub use shell::{ShellConfig, ShellTool};
pub use tool::{
    LockWaitHook, OutputChunk, OutputSink, OutputStream, Risk, Tool, ToolContext, ToolDetails,
    ToolOutput, ToolSpec, lenient_opt_u64,
};
pub use truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits};
pub use workspace::{
    FileAccess, FileGuard, FileStamp, FileTracker, LockManager, Workspace, WorkspaceGuard,
};
pub use write::WriteTool;
