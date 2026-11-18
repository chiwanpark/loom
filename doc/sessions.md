# Sessions

## Session states

| State | Meaning |
| --- | --- |
| Thinking | The model sends its reasoning, or it did not send output yet. |
| Responding | The model sends its answer: text or tool calls. |
| Running a tool | The agent runs tool calls. This includes the time when a call waits for a different session to release a file or the workspace. |
| Waiting for approval | A tool call waits for your approval. |
| Completed | The turn is complete, but you did not open the session after it. |
| Idle | You opened the completed session. A turn that failed or that you stopped also makes the session idle. |

A completed session becomes idle when you show it in a client and the window of the client is active. The agent tells all clients.

## Messages during a turn

You can write a message while the agent works. During a turn, the prompt box has two send actions:

- **Queue** (`enter`): The message waits until the turn is complete. Then it starts the next turn. Each queued message starts one turn, in sequence.
- **Steer** (`ctrl-enter`, `cmd-enter` on macOS, or the **Steer** button): The message goes into the running turn at its next step. The next step is after the results of the current tool calls. If the model answered without tool calls, the next step is immediately after the answer. Then the turn continues.

The waiting messages show above the prompt box. To edit a waiting message, click its **×**. The message goes back into the prompt box.

The session keeps the waiting messages, also when the agent restarts. If a turn fails or you stop it, the messages wait until the next turn is complete. Thus, nothing runs automatically after you stop a turn.

The Android app has the same **Queue** and **Steer** buttons.

## Clear a conversation

To start a conversation again, click **Clear** below the prompt box. The agent deletes the conversation and its history permanently.

These items stay:

- The name, the model, and the reasoning effort of the session.
- The tasks of the session.
- The tool approvals that you gave to the session.
- The notes of the agent.

## Rewind a conversation

To go back to the point before one of your messages:

- On the desktop, click the rewind button on the message.
- In the Android app, long-press the message and choose **Rewind to here**.

The agent deletes the message and all messages after it. The message goes back into the prompt box. You can edit it and send it again. All clients of the agent show the shorter conversation.

A rewind changes only the conversation:

- Files in the workspace do not change. The agent does not undo the changes of the deleted turns.
- The tasks, the notes of the agent, the settings of the session, and the waiting messages do not change.

Limits:

- You cannot rewind during a turn.
- After a compaction, you can rewind only to the last message before the summary, or to a message after the summary.
- You cannot undo a rewind.

## System prompt

To see what the model gets in addition to the conversation, do one of these steps:

- In the Sessions panel, choose **System prompt** in the menu of a session.
- In the command palette, choose **session: show system prompt**.
- In the Android app, choose **System prompt** in the menu of a session.

A tab shows the system prompt of the next model call of the session. It contains:

- The built-in instructions, or `instructions.system_prompt`.
- The `AGENTS.md` files.
- The environment.

Problems with the instructions show above the prompt. An example is an `AGENTS.md` file that is too large.

**Tools** shows the tools that the model can use. Click a tool to see its description and the JSON schema of its parameters.

The refresh button reads the prompt again. For example, use it after you change an `AGENTS.md` file.

## Sessions in a worktree

A session can work in its own git worktree. Its changes then stay away from the workspace and from other sessions until you merge them. The workspace must be in a git repository.

### Start a session in a worktree

- On the desktop, click **+** next to the host, and then click **In a worktree…** next to the workspace.
- Or, on the desktop, choose **New session in a worktree…** in the menu of an agent under **Remotes**.
- In the Android app, press the branch button next to the agent when you start a session.

Type a branch name and a base, or keep the defaults. The default branch is `loom/<session>`. The default base is the branch of the workspace.

### How a worktree session works

The agent adds the worktree in `<worktrees.dir>/<workspace-hash>/<session>`. The directories have mode 0700.

- The tools, the shell commands, `AGENTS.md`, and the skills of the session use the worktree.
- The system prompt tells the branch to the model.
- The Files and Changes panels and new terminals use the worktree of the active session.
- The Changes panel also shows the commits of the branch that are not in its base.
- The Sessions panel and the Android app show the branch after the title of the session.
- All sessions of the agent use the same notes. This also applies to sessions in worktrees.
- Shell commands in a worktree get the env file of the workspace, with the same redaction. See [Environment variables and secrets](environment.md).
- With the sandbox on, commands can also write the git directory of the repository. Thus, they can commit.

### Finish the work

Use the menu of the session. On the desktop, this menu is in the Sessions panel. On Android, it is the menu of the conversation.

- **Merge worktree** merges the branch into its base in the workspace.
  - The workspace must have the base checked out. Tracked files must have no uncommitted changes.
  - With a commit message, the agent squashes the branch into one commit. Without a commit message, it makes a merge commit that keeps the commits of the branch.
  - If the merge has conflicts, the workspace does not change, and the error shows the files. Resolve the conflicts in the worktree, for example: merge the base into the branch there. Then merge again.
- **Open pull request** pushes the branch to the remote of the base. If the base has no remote, it uses `origin`. Then it runs `gh pr create` with your title, description, and draft selection.
  - The [GitHub CLI](https://cli.github.com/) (`gh`) must be installed and signed in on the agent host.
  - The session keeps the URL of the pull request. After this, the menu pushes new commits to the pull request and opens it in the browser.
- **Remove worktree** removes the worktree, its branch, and the session. A session cannot continue without its worktree. If the worktree has uncommitted changes or commits that are not merged, the dialog tells you that you lose them.

Other effects:

- When you delete a session, the agent also removes its worktree and its branch.
- When you archive a session, the worktree and the branch stay.
- After a merge or a pull request, the worktree and the session stay. You can continue to work in them.
- If the worktree directory is not there, the session cannot start turns. To delete the session, remove the worktree.

Worktrees are full checkouts of your repository. If a dotfile manager or a backup tool synchronizes `~/.config`, exclude `~/.config/loom/worktrees`. Or set `worktrees.dir` to a different directory.

## Notes and tasks

Each agent has notes, and each session has a task list. The agent uses them for its work.

- They stay when a conversation is compacted or cleared.
- They are not files in the workspace.
- The agent does not add them to the instructions of the model. The agent reads them when it needs them. Its instructions tell it to read them again after a compaction.

### Notes

The notes are a text scratchpad for findings, decisions, and open questions about the work in the workspace of the agent. All sessions of the agent share them. Thus, the next session can use the findings of a session. The notes stay when you delete a session.

The agent uses these tools:

- `notes_read` reads the notes.
- `notes_edit` changes a part of the notes with a patch.
- `notes_write` replaces all of the notes.

`notes_write` does not replace notes that are not empty if the session did not read them after the last change. The last change can be by you, by a different session, or by a compaction or clear of the conversation. Thus, no change is lost without notice.

An agent from a version with notes for each session starts with the old notes of all its sessions. Each part has a heading with the title of the session.

### Tasks

The task list is the plan of the work. It has numbered tasks. The status of a task is created, in progress, or done.

- `tasks_read` reads the task list.
- `tasks_update` adds, changes, and removes tasks. It applies all changes of a batch, or no change.

### Notes and Tasks tabs

The **Notes** tab of the right panel shows the notes of the agent of the active session. To open it, press `ctrl-shift-n` or choose **panel: notes** in the command palette. You can edit the notes there:

- **Save** (`ctrl-s`) keeps your text.
- **Revert** deletes your changes.
- **Clear notes** deletes all text of the notes.

If a session changes the notes while you have unsaved changes, your changes stay. The tab lets you load the text of the agent or replace it with your text.

The **Tasks** tab shows the task list. To open it, press `ctrl-shift-t` or choose **panel: tasks**. Each task shows its number, such as `#3`. The agent uses this number to identify the task. When you remove tasks, the other tasks keep their numbers.

- To add a task, type it in the box at the bottom and press `enter`. `shift-enter` starts a new line.
- To change the status, click the circle of the task. The status changes from created to in progress to done.
- To edit a task, click its text. `enter` saves the change and `escape` cancels it.
- To remove a task, click the trash icon.
- To remove all tasks that are done, click **Clear done**.

Your changes apply immediately. If you and the agent change the list at the same time, the later change is kept.
