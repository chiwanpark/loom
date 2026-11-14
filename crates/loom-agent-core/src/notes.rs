//! The notes: a text scratchpad for the work in the agent's workspace,
//! which all of its sessions share. They stay when a conversation is
//! compacted or cleared, and the user can read and edit them too.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use loom_proto::{FileVersion, NotesContent, SessionId};
use loom_tools::patch::{apply_hunks, parse_hunks};
use loom_tools::{
    NotesAccess, Risk, Tool, ToolContext, ToolDetails, ToolError, ToolOutput, ToolSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::agent::AgentNotice;
use crate::error::{CoreError, Result};
use crate::runtime::RuntimeCell;
use crate::store::EventStore;

pub const NOTES_READ_TOOL: &str = "notes_read";
pub const NOTES_EDIT_TOOL: &str = "notes_edit";
pub const NOTES_WRITE_TOOL: &str = "notes_write";

/// Whether `tool` only touches the notes.
pub fn is_notes_tool(tool: &str) -> bool {
    [NOTES_READ_TOOL, NOTES_EDIT_TOOL, NOTES_WRITE_TOOL].contains(&tool)
}

/// The version clients see of notes with `text`: a SHA-256 hash of it.
pub fn version(text: &str) -> FileVersion {
    FileVersion(Sha256::digest(text.as_bytes()).into())
}

const LABEL: &str = "the notes";

/// The agent's notes, shared by its sessions.
pub(crate) struct AgentNotes {
    state: Mutex<NotesState>,
    store: Option<EventStore>,
    notices: Option<broadcast::Sender<AgentNotice>>,
    runtime: RuntimeCell,
}

struct NotesState {
    text: String,
    /// Counts the changes, so a session can tell whether its conversation
    /// holds the notes as they are.
    revision: u64,
    /// The revision each session's conversation holds: the session's agent
    /// read or wrote the notes then, and its context was not cleared or
    /// compacted since. Only such a session may replace all of the notes.
    seen: HashMap<SessionId, u64>,
}

impl AgentNotes {
    pub(crate) fn new(
        text: String,
        store: Option<EventStore>,
        notices: Option<broadcast::Sender<AgentNotice>>,
        runtime: RuntimeCell,
    ) -> Self {
        Self {
            state: Mutex::new(NotesState {
                text,
                revision: 0,
                seen: HashMap::new(),
            }),
            store,
            notices,
            runtime,
        }
    }

    fn lock(&self) -> MutexGuard<'_, NotesState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn limit(&self) -> u64 {
        self.runtime.current().config.tools.notes.max_bytes.as_u64()
    }

    pub(crate) fn text(&self) -> String {
        self.lock().text.clone()
    }

    /// The notes with their version and size limit, for clients.
    pub(crate) fn content(&self) -> NotesContent {
        let text = self.text();
        NotesContent {
            version: version(&text),
            text,
            max_bytes: self.limit(),
        }
    }

    /// Replaces the text, refusing one over the limit, and tells whether it
    /// changed. The caller holds the lock, so the store and clients get the
    /// changes in the order they happen.
    fn replace(&self, state: &mut NotesState, text: String) -> Result<bool> {
        let max = self.limit();
        if text.len() as u64 > max {
            return Err(CoreError::NotesTooLarge {
                size: text.len(),
                max,
            });
        }
        if text == state.text {
            return Ok(false);
        }
        if let Some(store) = &self.store {
            store.save_agent_notes(&text);
        }
        if let Some(notices) = &self.notices {
            let _ = notices.send(AgentNotice::NotesChanged {
                version: version(&text),
            });
        }
        state.text = text;
        state.revision += 1;
        Ok(true)
    }

    /// Replaces the notes for the user, during turns too, and returns their
    /// new version. Refused when the text is over `tools.notes.max_bytes`,
    /// or when the notes no longer have the `expected` version. No session
    /// has seen the user's text yet.
    pub(crate) fn set(&self, text: String, expected: Option<FileVersion>) -> Result<FileVersion> {
        let mut state = self.lock();
        if expected.is_some_and(|expected| expected != version(&state.text)) {
            return Err(CoreError::NotesChanged);
        }
        let new_version = version(&text);
        self.replace(&mut state, text)?;
        Ok(new_version)
    }

    /// Forgets that the session's conversation holds the notes, as when it
    /// is cleared or compacted, or the session is removed.
    pub(crate) fn unsee(&self, session: &SessionId) {
        self.lock().seen.remove(session);
    }

    /// The notes as one session's notes tools see them.
    pub(crate) fn access(self: &Arc<Self>, session: SessionId) -> Arc<dyn NotesAccess> {
        Arc::new(SessionNotes {
            notes: self.clone(),
            session,
        })
    }
}

/// The notes as one session's tools see them: what its agent reads or writes
/// is in its conversation, so it counts as seen.
struct SessionNotes {
    notes: Arc<AgentNotes>,
    session: SessionId,
}

impl std::fmt::Debug for SessionNotes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionNotes").field(&self.session).finish()
    }
}

impl NotesAccess for SessionNotes {
    fn read(&self) -> String {
        let mut state = self.notes.lock();
        let revision = state.revision;
        state.seen.insert(self.session.clone(), revision);
        state.text.clone()
    }

    fn edit(
        &self,
        change: &mut dyn FnMut(&str) -> loom_tools::Result<String>,
    ) -> loom_tools::Result<String> {
        let mut state = self.notes.lock();
        let held = state.seen.get(&self.session) == Some(&state.revision);
        let text = change(&state.text)?;
        let changed = self
            .notes
            .replace(&mut state, text.clone())
            .map_err(|err| ToolError::failed(err.to_string()))?;
        // The agent saw only what it replaced, so whether it holds the notes
        // as they are stays as it was.
        if changed && held {
            let revision = state.revision;
            state.seen.insert(self.session.clone(), revision);
        }
        Ok(text)
    }

    fn write(&self, text: String) -> loom_tools::Result<()> {
        let mut state = self.notes.lock();
        let held = state.seen.get(&self.session) == Some(&state.revision);
        if !held && !state.text.is_empty() {
            return Err(ToolError::failed(
                "the notes have text you have not read: the user or another session changed them, \
                 or this conversation was compacted or cleared, since you last read them; read \
                 them with notes_read before replacing them, or change a part of them with \
                 notes_edit",
            ));
        }
        self.notes
            .replace(&mut state, text)
            .map_err(|err| ToolError::failed(err.to_string()))?;
        let revision = state.revision;
        state.seen.insert(self.session.clone(), revision);
        Ok(())
    }
}

/// The notes a store kept per session, before the agent had one set of
/// notes, merged into one text: each session's under a heading with its
/// title. Sessions without notes are left out.
pub(crate) fn merge_session_notes<'a>(
    sessions: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    sessions
        .into_iter()
        .filter(|(_, notes)| !notes.trim().is_empty())
        .map(|(title, notes)| format!("## Session: {title}\n\n{}\n", notes.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn notes(ctx: &ToolContext) -> Result<&Arc<dyn NotesAccess>, ToolError> {
    ctx.notes
        .as_ref()
        .ok_or_else(|| ToolError::failed("the notes are only available in a session"))
}

fn output(content: String, bytes: usize) -> ToolOutput {
    ToolOutput {
        content,
        is_error: false,
        details: ToolDetails::Notes { bytes },
    }
}

fn args<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|err| ToolError::InvalidArguments(err.to_string()))
}

pub struct NotesReadTool;

#[async_trait]
impl Tool for NotesReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_READ_TOOL.into(),
            description: "Read the notes: the scratchpad for the work in this workspace, which all sessions of this agent share and which stays when the conversation is compacted or cleared. The user or another session may have changed them since you last read them.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        let text = notes(ctx)?.read();
        let bytes = text.len();
        let content = if text.is_empty() {
            "The notes are empty.".to_owned()
        } else {
            text
        };
        Ok(output(content, bytes))
    }
}

pub struct NotesEditTool;

#[derive(Deserialize)]
struct EditArgs {
    patch: String,
}

#[async_trait]
impl Tool for NotesEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_EDIT_TOOL.into(),
            description: "Change a part of the notes, which all sessions of this agent share, with a patch in unified diff format, without file headers. Each hunk starts with an `@@ -<line>,<count> +<line>,<count> @@` line, and each of its lines starts with ' ' (unchanged), '-' (removed) or '+' (added). Hunks are placed by their unchanged and removed lines, which must match the current notes, so include a few unchanged lines around each change; line numbers only help choose between several matching places.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The hunks to apply to the notes, in unified diff format"
                    }
                },
                "required": ["patch"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let hunks = parse_hunks(&self::args::<EditArgs>(args)?.patch)?;
        let text = notes(ctx)?.edit(&mut |current| apply_hunks(current, &hunks, LABEL))?;
        Ok(output(
            format!("Edited the notes; they are {} bytes now.", text.len()),
            text.len(),
        ))
    }
}

pub struct NotesWriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    #[serde(alias = "content")]
    text: String,
}

#[async_trait]
impl Tool for NotesWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NOTES_WRITE_TOOL.into(),
            description: "Replace all of the notes, which all sessions of this agent share. Read them first: notes with changes you have not read, by the user or another session, are not replaced. Prefer notes_edit for changing a part.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The notes' new text"
                    }
                },
                "required": ["text"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let text = self::args::<WriteArgs>(args)?.text;
        let bytes = text.len();
        notes(ctx)?.write(text)?;
        Ok(output(format!("Wrote the notes ({bytes} bytes)."), bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_notes_merge_under_their_titles() {
        let merged = merge_session_notes([
            ("First", "- a\n- b\n\n"),
            ("Empty", " \n"),
            ("Second", "# Plan\n1. x"),
        ]);
        assert_eq!(
            merged,
            "## Session: First\n\n- a\n- b\n\n## Session: Second\n\n# Plan\n1. x\n"
        );
        assert_eq!(merge_session_notes([("A", "")]), "");
    }

    #[test]
    fn notes_tools_are_recognised() {
        for tool in [NOTES_READ_TOOL, NOTES_EDIT_TOOL, NOTES_WRITE_TOOL] {
            assert!(is_notes_tool(tool), "{tool}");
        }
        assert!(!is_notes_tool("edit"));
    }
}
