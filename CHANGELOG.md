# Changelog

All notable changes to Loom are listed here, newest first. Versions follow [Semantic Versioning](https://semver.org). While Loom is on 0.x, a minor version may include breaking changes.

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
