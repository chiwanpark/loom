# Changelog

All notable changes to Loom are listed here, newest first. Versions follow [Semantic Versioning](https://semver.org). While Loom is on 0.x, a minor version may include breaking changes.

## Unreleased

- Each session has notes and a task list, which stay when the conversation is compacted or cleared. The notes are a text scratchpad for the agent's findings and decisions; the agent uses them through the new `notes_read`, `notes_edit`, and `notes_write` tools, and you can read and edit them in the client's new **Notes** tab. The task list is the plan of the work, as tasks that are created, in progress, or done; the agent uses it through the new `tasks_read` and `tasks_update` tools, and you can add, edit, check off, and remove tasks in the new **Tasks** tab, which shows each task's number (`#3`) as the agent refers to it. `tools.notes.max_bytes` and `tools.tasks.max_items` in `agent.yaml` limit their size (64 KiB and 100 tasks by default). See [Notes and tasks](README.md#notes-and-tasks).
- The protocol version is now 9. Agents and clients of 0.1.0 (protocol 6) cannot connect to these, so upgrade both. Running agents keep their old version until you restart them.
- `loom-agent reload --workspace <dir> | --instance <id> | --all`, and **Reload configuration** in an agent's menu in the Agents panel, make running agents read `agent.yaml` again and say whether they reloaded it, found it unchanged, or rejected it. The client now shows configuration notices as sentences, such as which sections need a restart or why a file was rejected, instead of their raw form. See [Reloading the configuration](README.md#reloading-the-configuration).
- `limits.max_rounds_per_turn` in `agent.yaml` sets the most model calls the agent makes for one message (200 by default, as before; `0` for no limit). It was fixed at 200 before. A config reload applies it at once, also to a turn that is running.
- New keys for tabs, which also work in terminals: `ctrl-tab` / `ctrl-shift-tab` for the next / previous tab, `alt-1` to `alt-8` for that tab and `alt-9` for the last one, and `ctrl-shift-pagedown` / `ctrl-shift-pageup` to move a tab right / left. `ctrl-shift-b` shows the Agents panel, `ctrl-shift-n` the Notes panel, and `ctrl-shift-t` the Tasks panel.
- Releases include `loom-agent` and `loom-relay` for arm64 Linux (`aarch64-unknown-linux-musl`), statically linked like the x86-64 ones, and `scripts/install.sh` picks the archive for the host's architecture. The GUI is still x86-64 only.

## 0.1.0

The first release.
