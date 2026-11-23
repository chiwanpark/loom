//! The Rust core of the Loom Android client. Kotlin sees it through UniFFI:
//! [`Loom`] keeps the device's key and the paired hosts and connects to
//! agents; [`AgentConnection`] follows an agent's sessions and
//! conversations, built by the same view model as the desktop client's.
//!
//! Async calls run on the core's own Tokio runtime. Cancelling the Kotlin
//! coroutine that waits for one aborts it; [`AgentConnection::disconnect`] ends
//! a connection and its update loop.

uniffi::setup_scaffolding!();

mod agent;
mod changes;
mod error;
mod files;
mod highlight;
mod hosts;
mod oracle;
mod ports;
mod records;
mod runtime;
mod terminal;

pub use agent::AgentConnection;
pub use changes::{ChangeKind, DiffKind, DiffRow, DiffView, GitChange, GitChanges, SessionChange};
pub use error::LoomError;
pub use files::{DirectoryListing, EntryKind, FileEntry, FileView};
pub use highlight::{CodeLine, Highlight, Span};
pub use hosts::{CLIENT_NAME, Loom};
pub use oracle::{
    BaseBranch, Codespace, CodespaceSession, CodespaceStatus, CreatedCodespace, NewCodespaceForm,
    NewCodespaceRequest, OracleConnection, OracleLists, OracleModel, OracleModels, OraclePreset,
    OracleRepository, OracleWorkspace, PullRequestOutcome, RepositoryState, close_plan,
};
pub use ports::{Forward, HostPort};
pub use records::*;
pub use terminal::{
    KeyModifiers, Terminal, TerminalColor, TerminalCursor, TerminalCursorShape, TerminalFrame,
    TerminalKey, TerminalPlace, TerminalRun, TerminalSummary,
};
