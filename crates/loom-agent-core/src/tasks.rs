//! The task list: the plan of the work each session keeps, as tasks with a
//! status. It stays when the conversation is compacted or cleared, and the
//! user can change it too.

use std::sync::Arc;

use async_trait::async_trait;
use loom_proto::{Task, TaskChange, TaskStatus};
use loom_tools::{
    Risk, TasksAccess, Tool, ToolContext, ToolDetails, ToolError, ToolOutput, ToolSpec,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::error::{CoreError, Result};

pub const TASKS_READ_TOOL: &str = "tasks_read";
pub const TASKS_UPDATE_TOOL: &str = "tasks_update";

/// Whether `tool` only touches the session's own task list.
pub fn is_tasks_tool(tool: &str) -> bool {
    [TASKS_READ_TOOL, TASKS_UPDATE_TOOL].contains(&tool)
}

/// A session's task list with the number its next task gets, so that the
/// numbers of removed tasks are not reused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskList {
    pub tasks: Vec<Task>,
    pub next_id: u32,
}

impl Default for TaskList {
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            next_id: 1,
        }
    }
}

impl TaskList {
    /// Applies `changes` in order, all or none, keeping at most `max` tasks.
    pub fn apply(&mut self, changes: Vec<TaskChange>, max: u32) -> Result<()> {
        let mut next = self.clone();
        for change in changes {
            match change {
                TaskChange::Add { text, status } => {
                    let text = task_text(text)?;
                    if next.tasks.len() >= max as usize {
                        return Err(CoreError::TooManyTasks { max });
                    }
                    next.tasks.push(Task {
                        id: next.next_id,
                        text,
                        status,
                    });
                    next.next_id += 1;
                }
                TaskChange::Update { id, text, status } => {
                    let task = next
                        .tasks
                        .iter_mut()
                        .find(|task| task.id == id)
                        .ok_or(CoreError::NoSuchTask { id })?;
                    if let Some(text) = text {
                        task.text = task_text(text)?;
                    }
                    if let Some(status) = status {
                        task.status = status;
                    }
                }
                TaskChange::Remove { id } => {
                    let ix = next
                        .tasks
                        .iter()
                        .position(|task| task.id == id)
                        .ok_or(CoreError::NoSuchTask { id })?;
                    next.tasks.remove(ix);
                }
            }
        }
        *self = next;
        Ok(())
    }
}

fn task_text(text: String) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CoreError::EmptyTask);
    }
    Ok(text.to_owned())
}

/// The task list as the model reads it: one task per line, with its number
/// and status; later lines of a task are indented.
pub fn render(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "The task list is empty.".to_owned();
    }
    let mut out = String::new();
    for task in tasks {
        let mut lines = task.text.lines();
        let first = lines.next().unwrap_or_default();
        out.push_str(&format!(
            "#{} [{}] {first}\n",
            task.id,
            task.status.as_str()
        ));
        for line in lines {
            out.push_str(&format!("    {line}\n"));
        }
    }
    out
}

fn tasks(ctx: &ToolContext) -> Result<&Arc<dyn TasksAccess>, ToolError> {
    ctx.tasks
        .as_ref()
        .ok_or_else(|| ToolError::failed("the task list is only available in a session"))
}

fn output(tasks: &[Task], prefix: &str) -> ToolOutput {
    ToolOutput {
        content: format!("{prefix}{}", render(tasks)),
        is_error: false,
        details: ToolDetails::Tasks { tasks: tasks.len() },
    }
}

pub struct TasksReadTool;

#[async_trait]
impl Tool for TasksReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TASKS_READ_TOOL.into(),
            description: "Read this session's task list: the plan of the work, as numbered tasks with a status (created, in_progress, or done). It stays when the conversation is compacted or cleared, and the user may have changed it since you last read it.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::ReadOnly
    }

    async fn call(&self, ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(output(&tasks(ctx)?.read(), ""))
    }
}

pub struct TasksUpdateTool;

#[derive(Deserialize)]
struct UpdateArgs {
    changes: Vec<ChangeArgs>,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ChangeArgs {
    Add {
        text: String,
        #[serde(default, deserialize_with = "status")]
        status: Option<TaskStatus>,
    },
    Update {
        #[serde(deserialize_with = "task_id")]
        id: u32,
        #[serde(default)]
        text: Option<String>,
        #[serde(default, deserialize_with = "status")]
        status: Option<TaskStatus>,
    },
    Remove {
        #[serde(deserialize_with = "task_id")]
        id: u32,
    },
}

impl From<ChangeArgs> for TaskChange {
    fn from(args: ChangeArgs) -> Self {
        match args {
            ChangeArgs::Add { text, status } => TaskChange::Add {
                text,
                status: status.unwrap_or_default(),
            },
            ChangeArgs::Update { id, text, status } => TaskChange::Update { id, text, status },
            ChangeArgs::Remove { id } => TaskChange::Remove { id },
        }
    }
}

/// A status as models write it: `in_progress`, `in-progress`, or
/// `In Progress` all mean the same.
fn status<'de, D: Deserializer<'de>>(de: D) -> Result<Option<TaskStatus>, D::Error> {
    let Some(text) = Option::<String>::deserialize(de)? else {
        return Ok(None);
    };
    let key: String = text
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    match key.as_str() {
        "created" => Ok(Some(TaskStatus::Created)),
        "inprogress" => Ok(Some(TaskStatus::InProgress)),
        "done" => Ok(Some(TaskStatus::Done)),
        _ => Err(serde::de::Error::custom(format!(
            "unknown status {text:?}; use created, in_progress, or done"
        ))),
    }
}

/// A task number, also written as a string such as `"3"` or `"#3"`.
fn task_id<'de, D: Deserializer<'de>>(de: D) -> Result<u32, D::Error> {
    match Value::deserialize(de)? {
        Value::Number(n) => n
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| serde::de::Error::custom(format!("bad task number {n}"))),
        Value::String(s) => s
            .trim()
            .trim_start_matches('#')
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("bad task number {s:?}"))),
        other => Err(serde::de::Error::custom(format!("bad task number {other}"))),
    }
}

#[async_trait]
impl Tool for TasksUpdateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TASKS_UPDATE_TOOL.into(),
            description: "Change this session's task list: add tasks, change a task's text or status, or remove tasks. The changes apply in order and all together; if one fails, none applies. Returns the new list.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "changes": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {
                                    "type": "string",
                                    "enum": ["add", "update", "remove"],
                                    "description": "add a task at the end, update a task's text or status, or remove a task"
                                },
                                "id": {
                                    "type": "integer",
                                    "description": "The task's number, for update and remove"
                                },
                                "text": {
                                    "type": "string",
                                    "description": "The task's text, for add (required) and update"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["created", "in_progress", "done"],
                                    "description": "The task's status; add defaults to created"
                                }
                            },
                            "required": ["action"]
                        }
                    }
                },
                "required": ["changes"]
            }),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Write
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> Result<ToolOutput, ToolError> {
        let args: UpdateArgs = serde_json::from_value(args)
            .map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
        if args.changes.is_empty() {
            return Err(ToolError::InvalidArguments(
                "changes must contain at least one change".into(),
            ));
        }
        let changes = args.changes.into_iter().map(TaskChange::from).collect();
        let list = tasks(ctx)?.update(changes)?;
        Ok(output(&list, "Updated the task list:\n"))
    }
}

/// Tells the model what the notes and the task list are for. It is fixed
/// text rather than their contents, so the system prompt stays the same while
/// they change and the prompt cache keeps working.
pub(crate) const PROMPT_SECTION: &str = "\
# Notes and tasks

This session has notes and a task list, which stay when the conversation is \
compacted or cleared. The user can read and change them too.
- The notes are a text scratchpad for findings, decisions, and open \
questions. Read them with notes_read; notes_edit changes a part of them, \
notes_write replaces all of them.
- The task list is the plan of the work: numbered tasks with a status of \
created, in_progress, or done. Read it with tasks_read and change it with \
tasks_update. For work with several steps, add the steps as tasks, mark a \
task in_progress when you start it and done when it is finished.
- Read both when you start a task, and again after the earlier conversation \
was summarized.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::{NOTES_EDIT_TOOL, NOTES_READ_TOOL, NOTES_WRITE_TOOL};

    fn add(text: &str) -> TaskChange {
        TaskChange::Add {
            text: text.into(),
            status: TaskStatus::Created,
        }
    }

    #[test]
    fn the_prompt_names_every_tool() {
        for tool in [
            NOTES_READ_TOOL,
            NOTES_EDIT_TOOL,
            NOTES_WRITE_TOOL,
            TASKS_READ_TOOL,
            TASKS_UPDATE_TOOL,
        ] {
            assert!(PROMPT_SECTION.contains(tool), "{tool}");
        }
    }

    #[test]
    fn numbers_are_not_reused() {
        let mut list = TaskList::default();
        list.apply(vec![add("a"), add(" b \n")], 10).unwrap();
        list.apply(vec![TaskChange::Remove { id: 2 }, add("c")], 10)
            .unwrap();
        let tasks: Vec<_> = list.tasks.iter().map(|t| (t.id, t.text.as_str())).collect();
        assert_eq!(tasks, [(1, "a"), (3, "c")]);
    }

    #[test]
    fn a_failing_change_applies_none() {
        let mut list = TaskList::default();
        list.apply(vec![add("a")], 2).unwrap();
        let before = list.clone();
        let err = list
            .apply(
                vec![
                    TaskChange::Update {
                        id: 1,
                        text: None,
                        status: Some(TaskStatus::Done),
                    },
                    TaskChange::Remove { id: 7 },
                ],
                2,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::NoSuchTask { id: 7 }));
        assert_eq!(list, before);
        assert!(matches!(
            list.apply(vec![add("b"), add("c")], 2),
            Err(CoreError::TooManyTasks { max: 2 })
        ));
        assert!(matches!(
            list.apply(vec![add("  ")], 2),
            Err(CoreError::EmptyTask)
        ));
        assert_eq!(list, before);
    }

    #[test]
    fn renders_numbers_statuses_and_indented_lines() {
        let tasks = vec![
            Task {
                id: 1,
                text: "plan".into(),
                status: TaskStatus::Done,
            },
            Task {
                id: 4,
                text: "build\nwith tests".into(),
                status: TaskStatus::InProgress,
            },
        ];
        assert_eq!(
            render(&tasks),
            "#1 [done] plan\n#4 [in_progress] build\n    with tests\n"
        );
        assert_eq!(render(&[]), "The task list is empty.");
    }

    #[test]
    fn reads_changes_as_models_write_them() {
        let args: UpdateArgs = serde_json::from_value(json!({"changes": [
            {"action": "add", "text": "a"},
            {"action": "add", "text": "b", "status": "In Progress"},
            {"action": "update", "id": "#2", "status": "done"},
            {"action": "remove", "id": 3},
        ]}))
        .unwrap();
        let changes: Vec<TaskChange> = args.changes.into_iter().map(Into::into).collect();
        assert_eq!(
            changes,
            [
                add("a"),
                TaskChange::Add {
                    text: "b".into(),
                    status: TaskStatus::InProgress
                },
                TaskChange::Update {
                    id: 2,
                    text: None,
                    status: Some(TaskStatus::Done)
                },
                TaskChange::Remove { id: 3 },
            ]
        );
        assert!(
            serde_json::from_value::<UpdateArgs>(
                json!({"changes": [{"action": "add", "text": "a", "status": "later"}]})
            )
            .is_err()
        );
    }
}
