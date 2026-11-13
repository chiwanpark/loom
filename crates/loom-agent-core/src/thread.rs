//! The operation thread: a text board each session keeps for the agent's
//! notes on its work. It stays when the conversation is compacted or
//! cleared, and the user can read and edit it too.

pub const THREAD_READ_TOOL: &str = "thread_read";
pub const THREAD_EDIT_TOOL: &str = "thread_edit";
pub const THREAD_WRITE_TOOL: &str = "thread_write";

/// Tells the model what the thread is for. It is fixed text rather than the
/// thread itself, so the system prompt stays the same while the thread
/// changes and the prompt cache keeps working.
pub(crate) const PROMPT_SECTION: &str = "\
# Operation thread

This session has an operation thread: a text board for your notes on the work, \
such as the plan, progress, decisions, and open questions. It stays when the \
conversation is compacted or cleared, and the user can read and edit it too.
- Read it with thread_read when you start a task, and again after the earlier \
conversation was summarized.
- Keep it current as the work moves on: thread_edit changes a part of it, \
thread_write replaces all of it.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_every_thread_tool() {
        for tool in [THREAD_READ_TOOL, THREAD_EDIT_TOOL, THREAD_WRITE_TOOL] {
            assert!(PROMPT_SECTION.contains(tool), "{tool}");
        }
    }
}
