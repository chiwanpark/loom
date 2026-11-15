<p><img src="assets/logo.svg" alt="Loom logo" width="128" height="128"></p>

# Loom

Loom is a coding agent split into two parts. An **agent** runs on the machine that holds the code and does the work there: it talks to the model, reads and edits files, and runs shell commands. A **client** (the `loom` desktop app) connects to any number of agents on your own machine, over SSH, or through a relay, and shows their sessions, terminals, file changes, and diffs. An [Android app](#the-android-app) follows the sessions of agents it reaches through a relay.

Agents keep running when the client disconnects, so you can close the app, reopen it later or on another machine, and pick up the same sessions.

Loom is early software. Configuration formats and the protocol may still change between 0.x versions; [CHANGELOG.md](CHANGELOG.md) lists what changed in each release.

## Components

| Binary | Crate | Purpose |
| --- | --- | --- |
| `loom-agent` | `crates/loom-agent` | The agent daemon and its command-line tools. It serves one workspace directory. |
| `loom` | `crates/loom-gui` | The desktop client, built with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui). Linux and Wayland only. |
| `loom-relay` | `crates/loom-relay` | An optional WebSocket relay so clients can reach agents behind NAT without SSH. |
| Loom for Android | `android/` | An Android client for agents reached through a relay. Android 17 (API 37) or newer, arm64. |

The other crates are libraries: the agent core (`loom-agent-core`), model providers (`loom-llm`), tools and sandboxing (`loom-tools`), the wire protocol (`loom-proto`), transports (`loom-transport`), the client library (`loom-client`), the client-side view model shared by the clients (`loom-view-model`), the Rust core of the Android client with its Kotlin bindings (`loom-mobile`), configuration (`loom-config`), key handling (`loom-crypto`), terminal emulation (`loom-vt`), and OpenTelemetry export (`loom-telemetry`).

## Downloading

The [releases page](https://github.com/chiwanpark/loom/releases) has prebuilt archives for Linux:

- `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` holds the desktop client `loom` together with `loom-agent`, for x86-64. It needs glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, a Wayland session, and a Vulkan driver.
- `loom-agent-<version>-<arch>-unknown-linux-musl.tar.gz` and `loom-relay-<version>-<arch>-unknown-linux-musl.tar.gz` are statically linked and run on any Linux, such as the remote hosts you reach over SSH. `<arch>` is `x86_64` for x86-64 and `aarch64` for arm64.

Unpack an archive and put its binaries on your `PATH`, for example in `~/.local/bin`. `SHA256SUMS` on the release page lists the archive checksums. Every binary prints its version and the commit it was built from with `--version`. [RELEASING.md](RELEASING.md) describes how releases are made.

### Installing the agent or relay

`scripts/install.sh agent` installs `loom-agent` for the current user. It downloads the static binary for the host's architecture from the latest release, checks it against `SHA256SUMS`, puts it in `~/.local/bin`, and writes `~/.config/loom/agent.yaml` if there is none. Run it again to upgrade.

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s agent
```

To install on a host you reach over SSH, run it there from a checkout:

```sh
ssh devbox sh -s agent < scripts/install.sh
```

`scripts/install.sh relay` installs `loom-relay` the same way. Run as root, it installs for the whole system instead: the binary in `/usr/local/bin` and the configuration in `/etc/loom/relay.yaml`. With `--systemd` it also installs and starts the `loom-relay` system service, which runs the relay as an unprivileged user that systemd creates for it:

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sudo sh -s relay --systemd
```

Options go after the component, for example `| sh -s agent --version 0.2.0`:

- `--version <version>` installs that release instead of the latest.
- `--bin-dir <dir>` installs somewhere other than `~/.local/bin` (or `/usr/local/bin`).
- `--source` builds the binary with Cargo instead, for hosts without a prebuilt binary (anything but x86-64 or arm64 Linux). Without `--version` it builds the `main` branch.
- `--systemd` also installs and starts the systemd service: for the agent, the user service described under [Through a relay](#through-a-relay); for the relay, the system service, which needs root.
- `--no-config` leaves the configuration alone.

Agents and relays that are already running keep their version until you restart them (`loom-agent stop --all`, `systemctl restart loom-relay`); the installer says when that is the case, and restarts the services it installed. Non-interactive SSH commands often run without `~/.local/bin` on `PATH`, so set `remote_bin` for the host in `client.yaml` (see [Remote hosts over SSH](#remote-hosts-over-ssh)).

## Building

You need a recent stable Rust toolchain (edition 2024).

The agent and relay build with Cargo alone. The desktop client also needs `pkg-config` and the development files for fontconfig, freetype, xkbcommon, and wayland. To run it you need those libraries plus a Vulkan driver. On Debian or Ubuntu:

```sh
sudo apt install pkg-config libfontconfig-dev libfreetype-dev libxkbcommon-dev libwayland-dev libvulkan-dev
```

Build everything:

```sh
cargo build --release
```

Or install the binaries into `~/.cargo/bin`:

```sh
cargo install --path crates/loom-agent
cargo install --path crates/loom-gui      # installs `loom`
cargo install --path crates/loom-relay    # only if you run a relay
```

Install `loom-agent` on every machine where you want an agent to run, including remote hosts you reach over SSH. [`scripts/install.sh agent --source`](#installing-the-agent-or-relay) does this with Cargo, too.

### The Android app

The Android app is built from source; releases do not include it yet. CI builds a debug APK of every change (the `loom-android-debug` artifact of a CI run), but each run signs it with a new debug key, so one does not install over another. You need JDK 21, the Android SDK with platform 37, build tools 37.0.0, and NDK 30.0.16248370, Rust's `aarch64-linux-android` target, and [cargo-ndk](https://github.com/bbqsrc/cargo-ndk):

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_HOME=~/Android/Sdk
cd android && ./gradlew assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Gradle builds the Rust core (`crates/loom-mobile`) with cargo-ndk and generates its Kotlin bindings with UniFFI, so the app always matches the protocol of the agents built from the same tree.

Android replaces an installed app only with an APK signed by the same key. To sign every build, debug ones too, with your own key, put its details in `~/.config/loom-android/keystore.properties` (or the file that `LOOM_ANDROID_KEYSTORE_PROPERTIES` or `-Ploom.keystoreProperties` names):

```properties
storeFile=/home/me/.config/loom-android/release.jks
storePassword=…
keyAlias=loom
keyPassword=…
```

Without it, debug builds are signed with the SDK's debug key. Keep the key and its password safe: an APK signed by another key installs only after you uninstall the app, which deletes its pairings.

Notifications need a Firebase project. In the [Firebase console](https://console.firebase.google.com), create one, add an Android app with the package name `com.chiwanpark.loom`, and put its `google-services.json` in `~/.config/loom-android/` (or the file that `LOOM_ANDROID_GOOGLE_SERVICES` or `-Ploom.googleServices` names). Builds without it, such as CI's, work the same without notifications.

The app reaches agents through a relay only (see [Through a relay](#through-a-relay)). After pairing, its start screen lists every session of every agent on the paired hosts, grouped by what they are doing, and you can start, rename, archive, and delete sessions there. In a session you can read the conversation, open tool calls to see their arguments and output, approve or deny tool calls, send messages, stop a turn, pick the model, the reasoning effort, and what happens to tool calls that no policy rule matches (ask, allow, or deny, or the agent's default) from the row above the prompt box (which also shows the context used), compact or clear the conversation, queue messages or steer a running turn with them, and edit the agent's notes and the session's tasks. **Font** in the menu of the start screen or a session opens a screen with a preview, where you set the text size, from 80% to 130% of the system's; the size of the text in blocks (tool calls' arguments and output, and the reasoning of answers), from 70% to 130% of the app's text; the font of the app's text; and the font of code: tool arguments and output, code in answers, notes, and keys. Code is shown in JetBrains Mono, which the app bundles, unless you pick another. Besides the system's fonts, you can search all of Google Fonts by name, with filters for fonts with Hangul and, for text, by category; the code tab lists monospaced fonts only. Google Play services downloads a Google font when it is first shown. The app's list of Google Fonts is `android/app/src/main/assets/google-fonts.tsv`, which `scripts/fonts/generate.py` writes from the directory of the Google Play services font provider and the metadata of fonts.google.com; run it again to pick up new fonts. Scanning the QR code needs Google Play services too; pasting the link works without them.

The **+** button on the start screen asks where the session goes: to one of the connected agents, or **In another directory…** on one of their hosts. Type the directory (`~` is your home directory on the host) or pick one where you started sessions before or one next to a running agent; the host's agent starts an agent there, or uses the one that already runs there, and the session opens once the app reaches it through the relay. The directory has to be under one of the `allowed_roots` of the host's agent configuration, as for `loom-agent serve` and the desktop's **Start an agent**.

From a session's menu, **Changes** shows what changed in the agent's workspace, as the desktop's Changes panel does: the uncommitted changes in git, with a robot mark on the files the session changed too, and, on its second tab, the files the session changed with its tool calls. Tapping a file opens its diff, with the line numbers of the old and new file. **Files** browses the agent's workspace: directories come first, files that git ignores are dimmed, and tapping a file opens it with line numbers. The file viewer is read-only and shows at most the first 2 MiB of a file. Files and diffs are highlighted for their language by the grammars that [bat](https://github.com/sharkdp/bat) collects, and the wrap button of either viewer switches between wrapping long lines and scrolling them sideways.

**Terminal** in a session's menu opens a terminal on the agent's host, running your shell in the agent's workspace, as the desktop's terminals do. A row above the keyboard has the keys a phone's keyboard lacks: Esc, Tab, arrows, Home, End, Page Up and Down, a few symbols, and Ctrl and Alt, which apply to the next key; a hardware keyboard works too. Drag to scroll back, pinch to change the text size, and use the menu to paste, copy the screen, switch between terminals, or open another. A terminal fits the screen as the keyboard comes and goes or the phone turns, and it keeps running when the link drops: the app takes it back when it reconnects, unless the agent closed it after `terminal.detached_timeout`.

The app keeps its links to the agents while it runs and reconnects by itself, catching open conversations up on what they missed. When it comes back on screen or the phone changes networks, it checks each link at once instead of waiting for the link's heartbeat to time out. A restarted agent runs as a new instance; the app finds it by its workspace on the host's relay, and open screens follow it. When the app and an agent speak different protocol versions, the app says which of the two to update.

Built with a Firebase project, the app shows a notification when a tool call waits for approval and when a turn finishes or fails, also while it is closed; tapping one opens the session, and none comes for the session on screen. Each kind has its own channel in the system's notification settings. The app tells each paired host where to send them when it connects; the host's agents seal every notification for the phone with the keys from pairing and hand it to their relay, which sends it through Firebase Cloud Messaging, so Google sees only ciphertext. The relay needs a key of the same Firebase project (see [Through a relay](#through-a-relay)).

## Getting started

1. Create the agent configuration:

   ```sh
   loom-agent config init     # writes ~/.config/loom/agent.yaml
   ```

2. Start the client with `loom`. It lists the hosts from `~/.config/loom/client.yaml`. The default is just your own machine (`local`).

3. Start an agent for a project directory: use **Start an agent** on a host in the agents panel, or run it yourself:

   ```sh
   loom-agent serve --workspace ~/src/my-project
   ```

   The client finds running agents on its own. For the local host it starts `loom-agent` from the same directory as `loom`, or from `PATH`. Set `LOOM_AGENT_BIN` to use a different binary.

4. Sign in to a model provider from the agent's menu (**Providers and sign-in…**), then start a session with `ctrl-n`.

### Remote hosts over SSH

Use **Add an SSH host** in the agents panel, or add an entry to `client.yaml`:

```yaml
hosts:
- kind: local
  name: local
- kind: ssh
  name: devbox
  destination: me@devbox.example.com
  remote_bin: /home/me/.cargo/bin/loom-agent   # optional; defaults to `loom-agent` on PATH
```

The client runs `ssh <destination> loom-agent ps --json` to list agents and `loom-agent stdio …` to connect, reusing one SSH connection per host. Non-interactive SSH sessions often do not include `~/.cargo/bin` in `PATH`; set `remote_bin` if `loom-agent` is not found.

### Through a relay

A relay lets clients reach agents that have no inbound SSH access. Both sides dial out to the relay, and the traffic between client and agent is end-to-end encrypted.

1. Run `loom-relay serve` somewhere both sides can reach (`loom-relay config init` writes a starting configuration). `sudo scripts/install.sh relay --systemd` installs it as a system service with its configuration in `/etc/loom/relay.yaml`. Set `allowed_hosts` there to the `host_id` that `loom-agent host` prints on each agent's host, then `systemctl restart loom-relay`.
2. In the agent's `agent.yaml`, set `relay.enabled: true` and `relay.url`.
3. On the agent's host, run `loom-agent pair`. It prints a one-time `loom://` link (and a QR code) that expires after 10 minutes by default.
4. In the client, use **Pair through a relay** and paste the link. In the Android app, use **Pair with a host** and scan the QR code or paste the link.

`loom-agent clients list` shows the paired clients, and `loom-agent clients revoke <label>` removes one.

For the Android app's notifications, the relay needs the private key of a service account of the app's Firebase project: in the Firebase console, open **Project settings**, then **Service accounts**, and use **Generate new private key** (the language picked for the code sample there does not matter). Keep the JSON file on the relay's host, readable by the relay only, and name it in `relay.yaml`:

```yaml
push:
  fcm:
    service_account: /etc/loom/firebase-service-account.json
```

The relay system service cannot read files that only root may read; `contrib/systemd/loom-relay.service` shows how to hand it the key as a credential. Agents send notifications through relays of this version or newer; with an older relay they connect as before, without them, and once the relay is upgraded they send them when they reconnect after its restart, without being restarted themselves. The relay's metrics count the notifications it sent (`loom_relay_pushes_sent_total`), sent to tokens Firebase no longer knows (`loom_relay_pushes_rejected_total`, which the agents then forget), could not send, and dropped.

To keep an agent running for the relay, `contrib/systemd/loom-agent.service` starts `loom-agent serve --default` (the `lifecycle.default_workspace` directory) as a systemd user service. `scripts/install.sh agent --systemd` installs and starts it; manual steps are in the file. To keep it running after you log out, run `loginctl enable-linger`.

## Model providers

| Provider | Sign-in |
| --- | --- |
| `anthropic` | Claude Pro/Max account (browser) |
| `openai` | ChatGPT account (browser) or API key (`OPENAI_API_KEY`) |
| `openai-codex` | ChatGPT account (browser or device code) |
| Any OpenAI-compatible server | API key; configure it under `llm.providers` |

Credentials are stored on the agent's host in `credentials.json` in the data directory (`storage.data_dir`, by default `~/.local/share/loom`). An OpenAI-compatible server, such as a local model, looks like this:

```yaml
llm:
  default_model: local/qwen3-coder
  providers:
    local:
      type: openai-compatible
      base_url: http://127.0.0.1:11434/v1
      api: chat
      models: [qwen3-coder]
```

## Agent configuration

The agent reads `~/.config/loom/agent.yaml`. Use `--config <path>` or `LOOM_CONFIG` to read a different file.

```sh
loom-agent config path     # which file is used
loom-agent config check    # validate it
loom-agent config schema   # print the JSON Schema
```

The main sections:

- `allowed_roots`: directories an agent may serve as a workspace (default `~`).
- `llm`: the default model and provider settings.
- `tools`: output limits for the `read` and `shell` tools, the shell program, the `search_web` tool (its model, source limits, and timeout), `notes.max_bytes`, the largest [notes](#notes-and-tasks) (64 KiB by default), and `tasks.max_items`, the most tasks in a task list (100 by default). `search_web` has an OpenAI model search the web. It uses the `openai-codex` sign-in and is not offered to the model without it.
- `policy`: whether each tool call is allowed, denied, or needs your approval (`ask`, the default). Rules can match a tool and a command prefix, for example to allow `git status` but deny `sudo`. The notes and tasks tools are allowed unless a rule says otherwise.
- `sandbox`: confines shell commands with `landlock` or `bubblewrap` (off by default), with optional network blocking and extra writable paths.
- `context`: tool output limits and when to compact a long conversation.
- `limits`: `max_rounds_per_turn`, the most model calls the agent makes for one message before the turn stops (200 by default; `0` for no limit). It guards against a model stuck in a loop while no one is watching. The conversation is kept, so you can send another message to go on.
- `instructions`: extra instructions for the model. `AGENTS.md` files in the workspace are included too.
- `relay`: relay connection settings.
- `telemetry`: OpenTelemetry (OTLP) export of metrics and events, off by default. `loom-agent config fetch-telemetry` prints this section from the settings your organization publishes for Claude Code.

`crates/loom-config/tests/fixtures/agent.yaml` is an example that uses most of these settings.

### Reloading the configuration

A running agent applies `agent.yaml` again when the file changes. To make it read the file on demand, for example when it missed a change or you want to know whether it accepted the file, run `loom-agent reload --workspace <dir> | --instance <id> | --all`, or choose **Reload configuration** in an agent's menu in the client's Agents panel. Each agent says whether it reloaded the file, found it unchanged, or rejected it; `loom-agent reload` exits with status 1 if any agent rejected the file or could not be reached.

An agent that rejects the file keeps the configuration it has, and every connected client shows why. Changes to `relay`, `socket`, `storage`, `lifecycle`, and `telemetry` take effect only when the agent restarts; the agent names the sections that are waiting for a restart.

## Agent commands

| Command | What it does |
| --- | --- |
| `loom-agent serve --workspace <dir>` | Run an agent for a workspace. `--default` serves `lifecycle.default_workspace`. |
| `loom-agent ps [--json]` | List the agents running on this host. |
| `loom-agent stop --workspace <dir> \| --instance <id> \| --all` | Stop agents gracefully. |
| `loom-agent reload --workspace <dir> \| --instance <id> \| --all` | Make agents read their configuration file again; see [Reloading the configuration](#reloading-the-configuration). |
| `loom-agent stdio --workspace <dir> [--spawn]` | Bridge stdin/stdout to an agent; this is what SSH connections use. `--spawn` starts the agent if none is running. |
| `loom-agent pair` | Print a one-time pairing link for a relay client. |
| `loom-agent clients list \| revoke <label>` | Manage clients paired through a relay. |
| `loom-agent host [--json]` | Print this host's ID and public key. |
| `loom-agent config init \| check \| schema \| path \| fetch-telemetry` | Manage the configuration. |

## Client configuration

The client reads `~/.config/loom/client.yaml`. Besides `hosts`, it has:

- `appearance`: theme, UI and monospace fonts and sizes, fallback fonts for glyphs the monospace font lacks (for example Hangul in terminals), and `wrap_lines`, whether file and diff views open with long lines wrapped (off by default).
- `keymap`: overrides for the default key bindings.

Useful default key bindings:

| Keys | Action |
| --- | --- |
| `ctrl-shift-p` | Command palette |
| `ctrl-n` | New session |
| `ctrl-p` | Find a session |
| `ctrl-alt-o` | Find an agent |
| `ctrl-shift-m` | Choose the model |
| `` ctrl-` `` | New terminal |
| `ctrl-b` | Show or hide the agents panel |
| `ctrl-shift-b` / `ctrl-shift-n` / `ctrl-shift-t` | Agents panel / the agent's notes / the session's tasks |
| `enter` / `ctrl-enter` while a turn runs | Queue the message for the next turn / steer the running turn with it |
| `ctrl-shift-e` / `ctrl-shift-g` | Workspace files / uncommitted changes |
| `ctrl-tab` / `ctrl-shift-tab` (or `ctrl-pagedown` / `ctrl-pageup`) | Next / previous tab in the pane |
| `alt-1` … `alt-8` / `alt-9` | Tab 1 to 8 / the last tab in the pane |
| `ctrl-shift-pagedown` / `ctrl-shift-pageup` | Move the tab right / left |
| `ctrl-w` | Close the tab |
| `ctrl-k` then an arrow | Split the pane |
| `ctrl-k ctrl-t` | Select a theme |
| `ctrl-s` | Save the file being edited in a file view, or the notes in the Notes tab |
| `alt-z` | Wrap long lines in a file or diff view, or stop wrapping them |

The tab and panel keys work in terminals too, so a terminal program does not see them (for example `alt-1` in readline). `ctrl-w`, `ctrl-b`, and `ctrl-p` go to the terminal.

To copy a workspace file to your machine, right-click it in the files panel and choose **Download…**, or use the download button in its file view. Downloads work over local and SSH connections, not through a relay yet.

When you open a session, the client loads its latest 50 messages and tool calls, and loads earlier ones as you scroll to the top. The **Changes** panel loads the whole session, to list every file it changed.

To start a session's conversation over, press **Clear** under the prompt box. The agent forgets the conversation and its history for good; the session keeps its name, model, reasoning effort, its tasks, and the tool approvals you gave it, and the agent's notes stay.

### Messages while a turn runs

You can keep typing while the agent works. Under a running turn the prompt box offers two ways to send:

- **Queue** (`enter`) waits for the turn to complete and then starts the next turn with your message. Several queued messages run one turn each, in order.
- **Steer** (`ctrl-enter`, `cmd-enter` on macOS, or the **Steer** button) puts your message into the running turn at its next step: after the results of the tool calls in progress, or, when the model answered without calling tools, right after the answer, and the turn goes on from there.

Waiting messages are listed above the prompt box; the **×** on one takes it back into the prompt box to edit. They are kept with the session, so they survive a restart of the agent. After a turn that you stop or that fails, they stay waiting until the next turn completes, so nothing runs on its own after you interrupt. The Android app has the same queue and steer buttons.

### Notes and tasks

Each agent has notes, and each session has a task list, for the agent's work. Both stay when a conversation is compacted or cleared. They are not files in the workspace, and their contents are not added to the model's instructions: the agent reads them when it needs to, and its instructions tell it to read them again after a long conversation is compacted.

- The **notes** are a text scratchpad for findings, decisions, and open questions about the work in the agent's workspace. All sessions of the agent share them, so what one session finds is there for the next, and they stay when a session is deleted. The agent reads them with `notes_read`, changes a part of them with `notes_edit`, which takes a patch, and replaces them with `notes_write`. An agent upgraded from a version with notes per session starts with the old notes of all its sessions, each under a heading with the session's title.
- The **task list** is the plan of the work: numbered tasks, each created, in progress, or done. The agent reads it with `tasks_read`, and adds, changes, and removes tasks with `tasks_update`, which applies a batch of changes all or none.

The **Notes** tab of the right panel (`ctrl-shift-n`, or **panel: notes** in the command palette) shows the notes of the active session's agent, and you can edit them there too: **Save** (`ctrl-s`) stores your text, **Revert** throws your changes away, and **Clear notes** empties them. When a session changes the notes while you have unsaved changes, yours stay and the tab offers to reload the agent's text or overwrite it with yours. So that changes are not lost unseen, `notes_write` refuses to replace non-empty notes that a session has not read since you or another session last changed them, or since its conversation was compacted or cleared.

The **Tasks** tab (`ctrl-shift-t`, or **panel: tasks**) shows the task list, with each task's number, such as `#3`, which the agent uses to refer to it. A task keeps its number when others are removed. Type a task in the box at the bottom and press `enter` to add it (`shift-enter` starts a new line). Click a task's circle to move it on from created to in progress to done, click its text to edit it (`enter` saves, `escape` cancels), and click the trash icon to remove it; **Clear done** removes the tasks that are done. Your changes apply at once, and when you and the agent change the list at the same time, the later change wins.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

## License

Loom is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE). Some code is adapted from [Zed](https://github.com/zed-industries/zed), and some themes come from [gpui-component](https://github.com/longbridge/gpui-component). Gotham is based on [vim-gotham](https://github.com/whatyouhide/vim-gotham), and the other themes are generated from the palettes at [TerminalColors](https://terminalcolors.com/). [NOTICE](NOTICE) lists the details and the authors of the color schemes.
