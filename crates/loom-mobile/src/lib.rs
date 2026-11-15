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
mod error;
mod files;
mod highlight;
mod hosts;
mod records;
mod runtime;

pub use agent::AgentConnection;
pub use error::LoomError;
pub use files::{DirectoryListing, EntryKind, FileEntry, FileView};
pub use highlight::{CodeLine, Highlight, Span};
pub use hosts::{CLIENT_NAME, Loom};
pub use records::*;
