# Changelog

All notable changes to Loom are listed here, newest first. Versions follow [Semantic Versioning](https://semver.org). While Loom is on 0.x, a minor version may include breaking changes.

## Unreleased

### Upgrade notes

- The protocol version is now 18. Clients and agents of 0.3.0 (protocol 14) cannot connect to this version. Upgrade the agents, the desktop app, and the Android app together.
- Shell commands and terminals no longer get the variables of the agent's environment whose names contain `KEY`, `SECRET`, `TOKEN`, or `PASSWORD`, such as `GITHUB_TOKEN`. To give a command such a variable, put it in the `.env` file of the workspace, or set `environment.ignore_default_excludes: true` in `agent.yaml`.
- If the workspace has a `.env` file, shell commands and terminals now get its variables. Set `environment.env_file: null` to stop this.

### New features

- **Rewind.** You can take a conversation back to before one of your messages. On the desktop, click the rewind button on the message. In the Android app, long-press the message and choose **Rewind to here**. The agent forgets the message and everything after it. The message goes back into the prompt box, so you can edit it and send it again. Files in the workspace do not change. You cannot rewind to a message that a compaction summarized. See [Rewinding a conversation](README.md#rewinding-a-conversation).
- **Environment variables and secrets.** The new `environment` section of `agent.yaml` sets the environment variables of shell commands and terminals. Shell commands and terminals also get the variables of the `.env` file in the workspace, for example the credentials of CLIs. The model does not see the values: they show as `[REDACTED:<NAME>]` in tool output, and the system prompt lists only the names. The agent cannot change the env file. See [Environment variables and secrets](README.md#environment-variables-and-secrets).
- **Sessions in a worktree.** A session can work in a git worktree of its own, on a new branch. When it is done, merge the branch into its base, squash it into one commit, or open a pull request with the GitHub CLI (`gh`) on the agent's host. The new `worktrees.dir` setting sets where the worktrees go (`~/.config/loom/worktrees` by default). The Files and Changes panels and new terminals follow the active session's worktree. See [Sessions in a worktree](README.md#sessions-in-a-worktree).
- **Committed changes.** The Changes panel of the desktop app now also shows the commits that the base branch does not have yet, with the files that they change. The base is the base branch of a worktree, or else the upstream branch. Click a file to see its diff in the commits.
- **Completed sessions.** A session has the new state **Completed** after a turn completes. The Sessions panel and the Android app show these sessions in their own group. When you open the session in a client, it becomes idle again.
- **Responding state.** A session has the new state **Responding** while the model streams text or tool calls. **Thinking** now means that the model streams its reasoning or has not sent output yet. Clients show "Responding…" while the model writes tool calls.
- **File upload.** The desktop app uploads files from your computer to the workspace. Drag files onto a folder in the files panel, or use **Upload files…**. Uploads also work through a relay. See [Client configuration](README.md#client-configuration).
- **System prompt viewer.** The desktop app and the Android app show the system prompt and the tools that a session sends to the model. In the desktop app, use **session: show system prompt** or the session's menu. In the Android app, use the menu of a session.
- **Self-update.** `loom-agent update` and `loom update` install the latest release in place of the running binaries. The desktop app also looks for a new release when it starts and once a day, and offers to install it and restart. Set `check_for_updates: false` in `client.yaml` to stop these checks. See [Updating](README.md#updating).
- **Agent update from the clients.** The desktop app and the Android app can update the agents of a host. A connected agent installs a release, by default the client's version. Then all agents of the host that run the same binary restart at once. Their running turns stop. A restarted agent keeps its PID, so systemd continues to track it. An agent also restarts this way when it receives `SIGUSR1`. See [Updating](README.md#updating).
- **Copy buttons for code and tables.** In the desktop app and the Android app, each code block in an answer has a copy button. Each table has a copy button that copies the table as Markdown.
- **Skills.** The agent supports skills in the [Agent Skills](https://agentskills.io) format. It finds them in `.loom/skills/` and `.agents/skills/` of the workspace, in `~/.config/loom/skills/` and `~/.agents/skills/`, and in `skills.paths`. The system prompt lists the skills. The model loads a skill with the new `skill` tool, which needs no approval. The `read` tool can read the files of the skills, also outside the workspace. Compaction keeps the instructions of the loaded skills. See [Skills](README.md#skills).
- **Desktop client installer.** `scripts/install.sh desktop` installs the desktop app `loom` and `loom-agent`. It also installs a desktop entry and icons, so that the application menu shows Loom. The release archive of the desktop app contains these files in its `share` directory.

### Changes

- **No lock waiting state.** A session that waits for another session to release a file or the workspace now shows **Running a tool**. Clients no longer show a separate message for this wait.
- The menu of a session in the Sessions panel no longer has **Close session**. To close a session's tabs, use **session: close** in the command palette.
- **Sessions panel.** The Agents panel of the desktop app is now the Sessions panel. It lists only sessions in its groups. An agent without sessions no longer shows as a "No sessions" row, and the **Offline** group is gone. The panel now shows the agents of each host under **Remotes**, with their state and menu. The commands are now **workspace: toggle sessions** and **panel: sessions**. Key bindings in `client.yaml` that use `loom::ToggleAgents` or `loom::ShowAgents` continue to work.
- **New session dialog.** The **+** button of a host now starts a session. The dialog lists the workspaces of the agents on the host. Click a workspace to start a session there, or **In a worktree…** to start one in a new worktree. If you type a new directory, the host starts an agent for it, and then the session starts.

## 0.3.0

### Upgrade notes

- The protocol version is now 14. Clients and agents of 0.2.0 (protocol 9) cannot connect to this version. Upgrade the agents, the desktop app, and the Android app together. A running agent keeps its old version until you restart it.
- Upgrade the relay to get notifications on the Android app. An older relay continues to work, but without notifications.
- Notes are now for each agent, not for each session. When an upgraded agent starts for the first time, it merges the notes of all its sessions into one set of notes. Each session's notes go under a heading with the session's title.

### New features

- **Android app.** The new app in `android/` connects to agents through a relay. To pair it with a host, scan the QR code from `loom-agent pair`. With the app, you can:
  - start, rename, archive, and delete sessions, also in a different directory of the host
  - follow a conversation, send messages, approve or deny tool calls, and stop a turn
  - change the model, the reasoning effort, and the default action for tool calls without a policy rule
  - compact or clear a conversation, and edit the notes and tasks
  - see the changes and their diffs, read the files of the workspace, and open terminals
  - set the text size, the fonts, and a fallback font for characters that a font does not have, for example Hangul

  The app connects again automatically after the network changes or an agent restarts. It needs Android 17 (API 37) or newer on arm64. For now, you must build the app yourself. CI builds a debug APK for each change. See [The Android app](README.md#the-android-app).
- **Notifications on Android.** The app shows a notification when a tool call needs approval, and when a turn finishes or fails. Tap the notification to open the session. The relay sends the notifications through Firebase Cloud Messaging. The agent encrypts each notification, so Google cannot read it. To turn on notifications, set `push.fcm.service_account` in `relay.yaml`. The relay metrics count the sent, failed, and dropped notifications.
- **Port forwarding.** The desktop app has a new **Ports** tab, and the Android app has a new **Ports** screen. They show the TCP ports that are open on the agent's host. You can forward a local port to one of these ports, also through a relay. On Android, the forwards continue when the app is in the background.
- **Image and PDF viewer.** The desktop app and the Android app show images and PDF files of up to 64 MB. In the desktop app, use the toolbar to zoom. In the Android app, pinch to zoom.
- **Shared terminals.** All clients of an agent show the terminals of that agent. A terminal is open on one client at a time. On other clients, the terminal is dimmed and has a **Take over** button. When you take over a terminal, you see its recent output (up to 128 KiB).
- **Messages during a turn.** Press `enter` to queue a message for the next turn. Press `ctrl-enter` or click **Steer** to add a message to the current turn. See [Messages while a turn runs](README.md#messages-while-a-turn-runs).
- **96 new themes** from [TerminalColors](https://terminalcolors.com/), for example Dracula, Gruvbox, Nord, Rosé Pine, and Tokyo Night Day. Press `ctrl-k ctrl-t` to select a theme. Terminals use the colors of the theme.

### Changes

- All sessions of an agent use the same notes. When you delete a session, the notes stay. `notes_write` does not replace notes that a different session changed after this session read them. See [Notes and tasks](README.md#notes-and-tasks).
- The `edit` tool now takes a patch in unified diff format. One patch can change, create, delete, and move many files. If a part of the patch does not apply, the tool changes no files. `notes_edit` also takes a patch.
- Long sessions open faster. A client loads only the last 50 messages and tool calls. It loads older items when you scroll up.
- The desktop app does not close the conversation tab of a session. To close a session, use **Close session** in the agents panel or **session: close** in the command palette.
- Downloads from the desktop app now also work through a relay.
- A relay now slows down a connection that sends too many messages. Before, it closed the connection. This disconnected all connections from the same IP address, for example all connections behind a Cloudflare Tunnel. `loom_relay_rate_limited_total` counts the slowed messages.

### Fixes

- `loom-agent config check` now fails when the configuration file does not exist. Before, it showed `ok`.
- Opening a long session through a relay no longer goes above the message limit of the relay. Before, this disconnected the agent from the relay.

## 0.2.0

- Each session has notes and a task list, which stay when the conversation is compacted or cleared. The notes are a text scratchpad for the agent's findings and decisions; the agent uses them through the new `notes_read`, `notes_edit`, and `notes_write` tools, and you can read and edit them in the client's new **Notes** tab. The task list is the plan of the work, as tasks that are created, in progress, or done; the agent uses it through the new `tasks_read` and `tasks_update` tools, and you can add, edit, check off, and remove tasks in the new **Tasks** tab, which shows each task's number (`#3`) as the agent refers to it. `tools.notes.max_bytes` and `tools.tasks.max_items` in `agent.yaml` limit their size (64 KiB and 100 tasks by default). See [Notes and tasks](README.md#notes-and-tasks).
- The protocol version is now 9. Agents and clients of 0.1.0 (protocol 6) cannot connect to these, so upgrade both. Running agents keep their old version until you restart them.
- `loom-agent reload --workspace <dir> | --instance <id> | --all`, and **Reload configuration** in an agent's menu in the Agents panel, make running agents read `agent.yaml` again and say whether they reloaded it, found it unchanged, or rejected it. The client now shows configuration notices as sentences, such as which sections need a restart or why a file was rejected, instead of their raw form. See [Reloading the configuration](README.md#reloading-the-configuration).
- `limits.max_rounds_per_turn` in `agent.yaml` sets the most model calls the agent makes for one message (200 by default, as before; `0` for no limit). It was fixed at 200 before. A config reload applies it at once, also to a turn that is running.
- New keys for tabs, which also work in terminals: `ctrl-tab` / `ctrl-shift-tab` for the next / previous tab, `alt-1` to `alt-8` for that tab and `alt-9` for the last one, and `ctrl-shift-pagedown` / `ctrl-shift-pageup` to move a tab right / left. `ctrl-shift-b` shows the Agents panel, `ctrl-shift-n` the Notes panel, and `ctrl-shift-t` the Tasks panel.
- Releases include `loom-agent` and `loom-relay` for arm64 Linux (`aarch64-unknown-linux-musl`), statically linked like the x86-64 ones, and `scripts/install.sh` picks the archive for the host's architecture. The GUI is still x86-64 only.

## 0.1.0

The first release.
