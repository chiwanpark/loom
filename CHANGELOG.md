# Changelog

All notable changes to Loom are listed here, newest first. Versions follow [Semantic Versioning](https://semver.org). While Loom is on 0.x, a minor version may include breaking changes.

## 0.8.0

### Upgrade notes

- **New layout file.** The desktop client does not read the tab layout of an earlier version. After the update, it starts with no open tabs one time. Open a codespace in the Codespaces panel to get its tabs again.
- **Archive calls of the oracle.** The oracle no longer accepts `archive_events` and `delete_archive` from devices. Only agents of the same workspace read an archive. Update the oracle, the agents, and the clients together.

### New features

- **Preset form.** The desktop app shows the `agent.yaml` of a preset in a form. You can set the model, the policy rules, the tools, the environment, the Git credentials, the secrets, and the other settings without YAML. **YAML** opens the text editor. See [Preset form](doc/desktop.md#preset-form).
- **Codespaces panel and session tabs.** The **Sessions** panel of the desktop client is now the **Codespaces** panel. It shows one row for each codespace, with what its sessions do. Each codespace has one group of tabs: its sessions, terminals, files, and diffs. A new session, also from a different client, gets a tab. See [Tabs of a codespace](doc/desktop.md#tabs-of-a-codespace).
- **Resume a session.** **Resume** below the prompt box lists the archived sessions of the codespace and the sessions of the closed codespaces of the same workspace. The agent copies a session from a closed codespace with its conversation and tasks. See [Resume a session](doc/desktop.md#resume-a-session).
- **Codespaces and tabs in the Android app.** The start screen of the Android app shows the codespaces. A codespace shows its sessions and terminals as tabs. The **Oracles** screen shows the status of each oracle. See [Sessions](doc/android.md#sessions).

### Changes

- **Closing a session tab archives the session.** An empty session is deleted instead. The clients no longer delete sessions, and the desktop client no longer shows the archive of a closed codespace. The command `session: close` is now `session: archive`.

## 0.7.1

### Fixes

- **Incus images on `dir` pools.** The images of `loom-host` and `loom-oracle` did not find a volume from a `dir` storage pool, and waited forever. The images now look for the mount in `/proc/self/mountinfo`.
- **Volume commands in the Incus guide.** The `incus storage volume attach` commands in [Incus deployment](doc/incus.md) did not give a device name, so Incus did not accept the path. The commands now give the device name.

## 0.7.0

### Upgrade notes

- **Incus images in the release.** The Incus images of `loom-host` and `loom-oracle` are now release assets. The CI artifact `incus-loom-host` is now `dist-incus`, and it has versioned file names.
- **Logs of the `loom-host` image.** The image now writes the log of `loom-host` to `log/loom-host.log` on its volume, not to the console. The container waits until a volume is attached at `/var/lib/loom-host`.

### New features

- **Incus image of the oracle.** The release has an Incus image of `loom-oracle` for amd64 and arm64. Run the oracle as a container, and keep its data on custom volumes. See [Incus deployment](doc/incus.md).
- **SSH to a `loom-host` container.** The Incus image of `loom-host` has an SSH server. Desktop clients can connect to codespaces over SSH also when `loom-host` runs in a container. The SSH keys can only run `loom-host stdio`. See [Incus deployment](doc/incus.md#ssh-connections).
- **Host links from the oracle machine.** `loom-oracle pair --host` makes a one-time link for a host. You do not need the desktop app for it.
- **`loom-oracle serve --wait`.** The oracle waits until `oracle.yaml` is ready and the master key can be used. Until then, it logs what is missing. See [Oracle](doc/oracle.md#set-up-an-oracle).
- **Host directories over HTTPS.** `loom-host` can now mount host directories, such as an NFS mount, also when it uses the HTTPS API of Incus. Incus enforces the paths and the ID mapping of the project. See [Host directories](doc/host.md#host-directories).

### Changes

- **`loom-host setup` with a certificate.** With `incus.url`, `loom-host setup` no longer fails. It shows the `incus` commands for the administrator: the project, the ID ranges, and the trust token.

## 0.6.0

### Upgrade notes

- **Loom-Next.** Loom now needs an oracle and an Incus host. Each session runs in a codespace. See [Oracle](doc/oracle.md) and [Hosts and codespaces](doc/host.md).
- **The classic modes are gone.** Loom no longer has local agents, agents over SSH, `loom-relay`, pairing with agents, or sessions in a git worktree. `loom-agent` has only `serve --managed`, `stdio --managed`, and `config`. The commands `ps`, `stop`, `reload`, `host`, `pair`, `clients`, and `update` are gone. An agent cannot update itself.
- `client.yaml` can have only hosts of the kind `oracle`. The desktop client ignores the hosts of the kinds `local`, `ssh`, and `relay`, and removes them from the file when it saves the file. The Android app removes these hosts. Pair the apps with the oracle again.
- `agent.yaml` has no `worktrees` section. Remove it from your presets.
- The oracle has no relay hosts.
- `scripts/install.sh` installs only `agent` and `desktop`, and has no `--systemd` option. The release has no `loom-relay` archives and no systemd unit for `loom-agent`.
- The protocol version is now 21. Clients and agents of 0.5.0 (protocol 19) cannot connect to this version. Upgrade the agents, the desktop app, and the Android app together.
- The agent does not load the sessions of earlier versions. It skips them and logs a warning. To remove them, delete the session stores in `~/.local/share/loom/sessions/`.
- `llm.default_reasoning_effort` is now `medium` if you do not set it. Before, the agent sent no effort, and the model used its own default. For Claude models, this turns on thinking. To turn off reasoning, set `llm.default_reasoning_effort: none`.

### New features

- **OpenCode Go.** Loom gets the current model list from OpenCode Go. Add an API key account for `opencode-go` on the **Models** page of the oracle settings.
- **GitHub Copilot.** Sign in with a GitHub device code and select a Copilot model that your account can use. See [Model providers](doc/configuration.md#model-providers).
- **Anthropic message threads.** Loom sends only new messages and the billing block when it can continue a thread. If the thread expires, Loom sends the full history again. See [Model providers](doc/configuration.md#model-providers).

### Changes

- **Session settings show their values.** The desktop app and the Android app showed **Default** for the reasoning effort, the tool call action, and the comment guard of a new session. Now they show the values that the session uses. For example, if `llm.default_reasoning_effort` is `high`, the apps show **High**.
- **No Default items.** The menus of the session settings no longer have a **Default** item. The model picker of the Android app no longer has the **Agent's default** item.
- **Settings of new sessions.** A new session gets its model, reasoning effort, policy action, and comment guard from the configuration of the agent. Changes to the configuration apply to new sessions. Sessions that exist keep their settings. See [Reload the configuration](doc/configuration.md#reload-the-configuration).

### Fixes

- **Anthropic billing version.** Loom reports Claude Code 2.1.323 by default. If Anthropic requires a higher billing version, Loom retries once with the version Anthropic names.
## 0.5.0

### Upgrade notes

- The protocol version is now 19. Clients and agents of 0.4.1 (protocol 18) cannot connect to this version. Upgrade the agents, the desktop app, and the Android app together.
- The comment guard is on by default. To turn it off, set `tools.comment_guard.mode: off` in `agent.yaml`.
- The `allow` policy action now asks for tool calls that may be harmful. To keep the old behavior, use the new `allow_harmful` action.

### New features

- **Allowed directories.** The new `tools.allowed_dirs` setting lists directories outside the workspace. The `read`, `edit`, and `write` tools can read and change the files in these directories. The system prompt tells the model about them. See [Agent configuration](doc/configuration.md#tools).
- **Comment guard.** The `edit` and `write` tools now reject changes that add comments to code. The guard allows changes to existing comments and new lines next to an existing comment. It also allows moved comments and tool directives, such as `eslint-disable`. The new `tools.comment_guard` section of `agent.yaml` sets the mode (`block`, `warn`, or `off`), comments to allow, and paths not to check. You can turn the guard on or off for one session in the status bar of the desktop app and in the Android app. See [Comment guard](doc/comment-guard.md).
- **Harmful tool calls.** The agent now checks `shell`, `edit`, and `write` calls that the policy allows. If a call may be harmful, the agent asks for your approval. Harmful calls change paths outside the workspace, the `.env` files, or the `.git` directory. Destructive git commands, such as `git reset --hard`, are harmful too. The new policy action `allow_harmful` allows these calls without approval. Changes in `sandbox.writable` are not harmful, also when the sandbox is off. Changes by the `edit` and `write` tools in `tools.allowed_dirs` are not harmful. The new `policy.harmful.deny_paths` setting adds paths whose changes are always harmful. See [Harmful tool calls](doc/policy.md#harmful-tool-calls).
- **Default reasoning effort.** The new `llm.default_reasoning_effort` setting sets the reasoning effort of new sessions. It also applies to sessions that use the **Default** effort. An effort that you pick for a session replaces it. See [Model providers](doc/configuration.md#model-providers).
- **Host colors.** You can give each host a color in the desktop app and the Android app. The session lists then show the name of each host in its color. On the desktop, click the palette button of the host under **Remotes**. In the Android app, choose **Color…** in the menu of the host on the **Hosts** screen. See [Host colors](doc/desktop.md#host-colors).

### Changes

- **Tools in the system prompt.** The system prompt no longer lists the names of the tools. The model gets the tools from the `tools` field of the API request.
- **Branch names in the Android app.** The start screen of the Android app shows the branch of a worktree session after the session's title, as the desktop app does. Before, it showed the branch after the workspace.
- **Documentation.** The README now has only the core guide. The detailed documentation is in the `doc` directory. The release archives include this directory.

### Fixes

- **Table cells in the Android app.** The Android app showed only one line of text in each table cell and cut the rest. Now long text wraps to more lines, and the app shows all of the text.

## 0.4.1

### Fixes

- **User attributes in telemetry.** Claude Code adds `user.email`, `user.account_uuid`, `user.account_id`, and `organization.id` to each metric and event. Loom did not add them, so dashboards that group by these attributes did not show Loom's data. Now Loom adds them too. The agent gets the values from the Anthropic sign-in. If the sign-in has no email, the agent uses `git config --global user.email`. To set the values yourself, use the new `telemetry` settings `user_email`, `account_uuid`, and `organization_id`. Set `include_account_uuid: false` to leave out the account. See [Telemetry](doc/telemetry.md).
- **Skill files.** The `skill` tool listed files that tools make, such as `__pycache__` directories and `*.pyc` files, among the files of a skill. Now the list does not include them. It also does not include `node_modules` and Python virtual environments.

## 0.4.0

### Upgrade notes

- The protocol version is now 18. Clients and agents of 0.3.0 (protocol 14) cannot connect to this version. Upgrade the agents, the desktop app, and the Android app together.
- Shell commands and terminals no longer get the variables of the agent's environment whose names contain `KEY`, `SECRET`, `TOKEN`, or `PASSWORD`, such as `GITHUB_TOKEN`. To give a command such a variable, put it in the `.env` file of the workspace, or set `environment.ignore_default_excludes: true` in `agent.yaml`.
- If the workspace has a `.env` file, shell commands and terminals now get its variables. Set `environment.env_file: null` to stop this.

### New features

- **Rewind.** You can take a conversation back to before one of your messages. On the desktop, click the rewind button on the message. In the Android app, long-press the message and choose **Rewind to here**. The agent forgets the message and everything after it. The message goes back into the prompt box, so you can edit it and send it again. Files in the workspace do not change. You cannot rewind to a message that a compaction summarized. See [Rewind a conversation](doc/sessions.md#rewind-a-conversation).
- **Environment variables and secrets.** The new `environment` section of `agent.yaml` sets the environment variables of shell commands and terminals. Shell commands and terminals also get the variables of the `.env` file in the workspace, for example the credentials of CLIs. The model does not see the values: they show as `[REDACTED:<NAME>]` in tool output, and the system prompt lists only the names. The agent cannot change the env file. See [Environment variables and secrets](doc/environment.md).
- **Sessions in a worktree.** A session can work in a git worktree of its own, on a new branch. When it is done, merge the branch into its base, squash it into one commit, or open a pull request with the GitHub CLI (`gh`) on the agent's host. The new `worktrees.dir` setting sets where the worktrees go (`~/.config/loom/worktrees` by default). The Files and Changes panels and new terminals follow the active session's worktree. See [Sessions in a worktree](doc/sessions.md#sessions-in-a-worktree).
- **Committed changes.** The Changes panel of the desktop app now also shows the commits that the base branch does not have yet, with the files that they change. The base is the base branch of a worktree, or else the upstream branch. Click a file to see its diff in the commits.
- **Completed sessions.** A session has the new state **Completed** after a turn completes. The Sessions panel and the Android app show these sessions in their own group. When you open the session in a client, it becomes idle again.
- **Responding state.** A session has the new state **Responding** while the model streams text or tool calls. **Thinking** now means that the model streams its reasoning or has not sent output yet. Clients show "Responding…" while the model writes tool calls.
- **File upload.** The desktop app uploads files from your computer to the workspace. Drag files onto a folder in the files panel, or use **Upload files…**. Uploads also work through a relay. See [Upload files](doc/desktop.md#upload-files).
- **System prompt viewer.** The desktop app and the Android app show the system prompt and the tools that a session sends to the model. In the desktop app, use **session: show system prompt** or the session's menu. In the Android app, use the menu of a session.
- **Self-update.** `loom-agent update` and `loom update` install the latest release in place of the running binaries. The desktop app also looks for a new release when it starts and once a day, and offers to install it and restart. Set `check_for_updates: false` in `client.yaml` to stop these checks. See [Updates](doc/install.md#updates).
- **Agent update from the clients.** The desktop app and the Android app can update the agents of a host. A connected agent installs a release, by default the client's version. Then all agents of the host that run the same binary restart at once. Their running turns stop. A restarted agent keeps its PID, so systemd continues to track it. An agent also restarts this way when it receives `SIGUSR1`. See [Agents of a host](doc/install.md#agents-of-a-host).
- **Copy buttons for code and tables.** In the desktop app and the Android app, each code block in an answer has a copy button. Each table has a copy button that copies the table as Markdown.
- **Skills.** The agent supports skills in the [Agent Skills](https://agentskills.io) format. It finds them in `.loom/skills/` and `.agents/skills/` of the workspace, in `~/.config/loom/skills/` and `~/.agents/skills/`, and in `skills.paths`. The system prompt lists the skills. The model loads a skill with the new `skill` tool, which needs no approval. The `read` tool can read the files of the skills, also outside the workspace. Compaction keeps the instructions of the loaded skills. See [Skills](doc/skills.md).
- **Desktop client installer.** `scripts/install.sh desktop` installs the desktop app `loom` and `loom-agent`. It also installs a desktop entry and icons, so that the application menu shows Loom. The release archive of the desktop app contains these files in its `share` directory.

### Changes

- **No lock waiting state.** A session that waits for another session to release a file or the workspace now shows **Running a tool**. Clients no longer show a separate message for this wait.
- The menu of a session in the Sessions panel no longer has **Close session**. To close a session's tabs, use **session: close** in the command palette.
- **Sessions panel.** The Agents panel of the desktop app is now the Sessions panel. It lists only sessions in its groups. An agent without sessions no longer shows as a "No sessions" row, and the **Offline** group is gone. The panel now shows the agents of each host under **Remotes**, with their state and menu. The commands are now **workspace: toggle sessions** and **panel: sessions**. Key bindings in `client.yaml` that use `loom::ToggleAgents` or `loom::ShowAgents` continue to work.
- **New session dialog.** The **+** button of a host now starts a session. The dialog lists the workspaces of the agents on the host. Click a workspace to start a session there, or **In a worktree…** to start one in a new worktree. If you type a new directory, the host starts an agent for it, and then the session starts.

### Fixes

- A terminal sometimes stayed open in a client after another client took the terminal and closed it at once. This occurred in the desktop app and in the Android app. Now the desktop app closes the terminal's tab, and the Android app shows that the terminal was closed on another client.

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

  The app connects again automatically after the network changes or an agent restarts. It needs Android 17 (API 37) or newer on arm64. For now, you must build the app yourself. CI builds a debug APK for each change. See [Android app](doc/android.md).
- **Notifications on Android.** The app shows a notification when a tool call needs approval, and when a turn finishes or fails. Tap the notification to open the session. The relay sends the notifications through Firebase Cloud Messaging. The agent encrypts each notification, so Google cannot read it. To turn on notifications, set `push.fcm.service_account` in `relay.yaml`. The relay metrics count the sent, failed, and dropped notifications.
- **Port forwarding.** The desktop app has a new **Ports** tab, and the Android app has a new **Ports** screen. They show the TCP ports that are open on the agent's host. You can forward a local port to one of these ports, also through a relay. On Android, the forwards continue when the app is in the background.
- **Image and PDF viewer.** The desktop app and the Android app show images and PDF files of up to 64 MB. In the desktop app, use the toolbar to zoom. In the Android app, pinch to zoom.
- **Shared terminals.** All clients of an agent show the terminals of that agent. A terminal is open on one client at a time. On other clients, the terminal is dimmed and has a **Take over** button. When you take over a terminal, you see its recent output (up to 128 KiB).
- **Messages during a turn.** Press `enter` to queue a message for the next turn. Press `ctrl-enter` or click **Steer** to add a message to the current turn. See [Messages during a turn](doc/sessions.md#messages-during-a-turn).
- **96 new themes** from [TerminalColors](https://terminalcolors.com/), for example Dracula, Gruvbox, Nord, Rosé Pine, and Tokyo Night Day. Press `ctrl-k ctrl-t` to select a theme. Terminals use the colors of the theme.

### Changes

- All sessions of an agent use the same notes. When you delete a session, the notes stay. `notes_write` does not replace notes that a different session changed after this session read them. See [Notes and tasks](doc/sessions.md#notes-and-tasks).
- The `edit` tool now takes a patch in unified diff format. One patch can change, create, delete, and move many files. If a part of the patch does not apply, the tool changes no files. `notes_edit` also takes a patch.
- Long sessions open faster. A client loads only the last 50 messages and tool calls. It loads older items when you scroll up.
- The desktop app does not close the conversation tab of a session. To close a session, use **Close session** in the agents panel or **session: close** in the command palette.
- Downloads from the desktop app now also work through a relay.
- A relay now slows down a connection that sends too many messages. Before, it closed the connection. This disconnected all connections from the same IP address, for example all connections behind a Cloudflare Tunnel. `loom_relay_rate_limited_total` counts the slowed messages.

### Fixes

- `loom-agent config check` now fails when the configuration file does not exist. Before, it showed `ok`.
- Opening a long session through a relay no longer goes above the message limit of the relay. Before, this disconnected the agent from the relay.

## 0.2.0

- Each session has notes and a task list, which stay when the conversation is compacted or cleared. The notes are a text scratchpad for the agent's findings and decisions; the agent uses them through the new `notes_read`, `notes_edit`, and `notes_write` tools, and you can read and edit them in the client's new **Notes** tab. The task list is the plan of the work, as tasks that are created, in progress, or done; the agent uses it through the new `tasks_read` and `tasks_update` tools, and you can add, edit, check off, and remove tasks in the new **Tasks** tab, which shows each task's number (`#3`) as the agent refers to it. `tools.notes.max_bytes` and `tools.tasks.max_items` in `agent.yaml` limit their size (64 KiB and 100 tasks by default). See [Notes and tasks](doc/sessions.md#notes-and-tasks).
- The protocol version is now 9. Agents and clients of 0.1.0 (protocol 6) cannot connect to these, so upgrade both. Running agents keep their old version until you restart them.
- `loom-agent reload --workspace <dir> | --instance <id> | --all`, and **Reload configuration** in an agent's menu in the Agents panel, make running agents read `agent.yaml` again and say whether they reloaded it, found it unchanged, or rejected it. The client now shows configuration notices as sentences, such as which sections need a restart or why a file was rejected, instead of their raw form. See [Reload the configuration](doc/configuration.md#reload-the-configuration).
- `limits.max_rounds_per_turn` in `agent.yaml` sets the most model calls the agent makes for one message (200 by default, as before; `0` for no limit). It was fixed at 200 before. A config reload applies it at once, also to a turn that is running.
- New keys for tabs, which also work in terminals: `ctrl-tab` / `ctrl-shift-tab` for the next / previous tab, `alt-1` to `alt-8` for that tab and `alt-9` for the last one, and `ctrl-shift-pagedown` / `ctrl-shift-pageup` to move a tab right / left. `ctrl-shift-b` shows the Agents panel, `ctrl-shift-n` the Notes panel, and `ctrl-shift-t` the Tasks panel.
- Releases include `loom-agent` and `loom-relay` for arm64 Linux (`aarch64-unknown-linux-musl`), statically linked like the x86-64 ones, and `scripts/install.sh` picks the archive for the host's architecture. The GUI is still x86-64 only.

## 0.1.0

The first release.
