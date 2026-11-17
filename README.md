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

- `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` holds the desktop client `loom` together with `loom-agent`, for x86-64. It needs glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, a Wayland session, and a Vulkan driver. Its `share` directory holds the desktop entry and icons, laid out as in `~/.local/share`.
- `loom-agent-<version>-<arch>-unknown-linux-musl.tar.gz` and `loom-relay-<version>-<arch>-unknown-linux-musl.tar.gz` are statically linked and run on any Linux, such as the remote hosts you reach over SSH. `<arch>` is `x86_64` for x86-64 and `aarch64` for arm64.

Unpack an archive and put its binaries on your `PATH`, for example in `~/.local/bin`. `SHA256SUMS` on the release page lists the archive checksums. Every binary prints its version and the commit it was built from with `--version`. [RELEASING.md](RELEASING.md) describes how releases are made.

### Installing the agent, relay, or desktop client

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

`scripts/install.sh desktop` installs the desktop client `loom` and the `loom-agent` it starts, for the current user, from the x86-64 archive. It also installs the desktop entry `dev.loom.Loom.desktop` in `~/.local/share/applications` and the icons in `~/.local/share/icons`, so that the desktop's application menu lists Loom:

```sh
curl -fsSL https://raw.githubusercontent.com/chiwanpark/loom/main/scripts/install.sh | sh -s desktop
```

Options go after the component, for example `| sh -s agent --version 0.2.0`:

- `--version <version>` installs that release instead of the latest.
- `--bin-dir <dir>` installs somewhere other than `~/.local/bin` (or `/usr/local/bin`).
- `--source` builds the binaries with Cargo instead, for hosts without a prebuilt binary (anything but x86-64 or arm64 Linux, or anything but x86-64 Linux for the desktop client). Without `--version` it builds the `main` branch. The desktop client needs the packages listed under [Building](#building).
- `--systemd` also installs and starts the systemd service: for the agent, the user service described under [Through a relay](#through-a-relay); for the relay, the system service, which needs root. The desktop client has no service.
- `--no-config` leaves the configuration alone.

Agents, relays, and desktop clients that are already running keep their version until you restart them (`loom-agent stop --all`, `systemctl restart loom-relay`); the installer says when that is the case for agents and relays, and restarts the services it installed. Non-interactive SSH commands often run without `~/.local/bin` on `PATH`, so set `remote_bin` for the host in `client.yaml` (see [Remote hosts over SSH](#remote-hosts-over-ssh)).

### Updating

The agent and the desktop client update themselves from the GitHub releases. They download the archive for the machine, check it against `SHA256SUMS`, make sure that each new binary runs, and then replace the binaries in the directory of the running binary. If a step fails, the old binaries stay.

- `loom-agent update` installs the latest release of `loom-agent`. `--check` only tells whether a newer release is out, and exits with status 1 if one is. `--version <version>` installs that release, also an older one. Running agents keep their version until you restart them (`loom-agent stop --all`).
- The desktop client looks for a newer release when it starts and once a day after that. When it finds one, it shows a notification with an **Update** button. The update replaces `loom` and the `loom-agent` next to it, and then the client offers to restart. To look for a release yourself, choose **app: check for updates** in the command palette. Set `check_for_updates: false` in `client.yaml` to stop the automatic checks. Debug builds never check by themselves.
- `loom update` updates the desktop client and its agent without opening a window, with the same `--check` and `--version` options.
- The desktop client and the Android app also update the agents of a host. In the desktop client, click the download button of the host under **Remotes**. In the Android app, use **Update agents…** in the menu of the host. The default version is the client's version. Clear it to install the latest release. A connected agent of the host installs the release in place of its binary. Then all agents of the host that run this binary restart at once, with their PIDs and arguments. Their running turns stop, and the clients connect to them again. Agents of another build that run from the same path keep their version until you restart them. An agent also restarts this way when it receives `SIGUSR1`. The client must speak the same protocol as the agent, so after an update that changes the protocol, update the agents with `loom-agent update` instead.

The updates do not change the desktop entry, the icons, or the systemd units; run `scripts/install.sh` again for those. The relay does not update itself. The directory of the binaries must be writable, so a system-wide install needs the installer with root.

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

Install `loom-agent` on every machine where you want an agent to run, including remote hosts you reach over SSH. [`scripts/install.sh agent --source`](#installing-the-agent-relay-or-desktop-client) does this with Cargo, too.

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

The app reaches agents through a relay only (see [Through a relay](#through-a-relay)). After pairing, its start screen lists every session of every agent on the paired hosts, grouped by what they are doing as in the desktop's Sessions panel, and you can start, rename, archive, and delete sessions there. A completed session becomes idle when you open it in the app. In a session you can read the conversation, open tool calls to see their arguments and output, approve or deny tool calls, send messages, stop a turn, pick the model, the reasoning effort, what happens to tool calls that no policy rule matches (ask, allow, allow harmful, or deny, or the agent's default), and the [comment guard](#comment-guard) from the row above the prompt box (which also shows the context used), compact or clear the conversation, rewind it to one of your messages (long-press the message; see [Rewinding a conversation](#rewinding-a-conversation)), queue messages or steer a running turn with them, and edit the agent's notes and the session's tasks. **Font** in the menu of the start screen or a session opens a screen with a preview, where you set the text size, from 80% to 130% of the system's; the size of the text in blocks (tool calls' arguments and output, and the reasoning of answers), from 70% to 130% of the app's text; the font of the app's text; and the font of code: tool arguments and output, code in answers, notes, and keys. Code is shown in JetBrains Mono, which the app bundles, unless you pick another. Besides the system's fonts, you can search all of Google Fonts by name, with filters by script (such as Korean, Japanese, or Chinese) and, for text, by category; the code tab lists monospaced fonts only. Each of the two fonts can also have a fallback font from Google Fonts, which draws the characters that the font does not have, such as Hangul in JetBrains Mono, before the system's fonts do. Google Play services downloads a Google font when it is first shown. The app's list of Google Fonts is `android/app/src/main/assets/google-fonts.tsv`, which `scripts/fonts/generate.py` writes from the directory of the Google Play services font provider and the metadata of fonts.google.com; run it again to pick up new fonts. Scanning the QR code needs Google Play services too; pasting the link works without them.

The **+** button on the start screen asks where the session goes: to one of the connected agents, or **In another directory…** on one of their hosts. Type the directory (`~` is your home directory on the host) or pick one where you started sessions before or one next to a running agent; the host's agent starts an agent there, or uses the one that already runs there, and the session opens once the app reaches it through the relay. The directory has to be under one of the `allowed_roots` of the host's agent configuration, as for `loom-agent serve` and the desktop's **New session** dialog.

From a session's menu, **Changes** shows what changed in the agent's workspace, as the desktop's Changes panel does: the uncommitted changes in git, with a robot mark on the files the session changed too, and, on its second tab, the files the session changed with its tool calls. Tapping a file opens its diff, with the line numbers of the old and new file. **Files** browses the agent's workspace: directories come first, files that git ignores are dimmed, and tapping a file opens it with line numbers. The file viewer is read-only and shows at most the first 2 MiB of a file. Images (PNG, JPEG, WebP, GIF, BMP, HEIF, and AVIF) and PDF files of up to 64 MiB open in a viewer instead, which copies them to the app's cache while it is open: pinch to zoom, pan a zoomed image with a finger, and double-tap an image to zoom it in or out; a zoomed PDF file scrolls sideways too. Files and diffs are highlighted for their language by the grammars that [bat](https://github.com/sharkdp/bat) collects, and the wrap button of either viewer switches between wrapping long lines and scrolling them sideways.

**Terminal** in a session's menu opens a terminal on the agent's host, running your shell in the agent's workspace, as the desktop's terminals do. A row above the keyboard has the keys a phone's keyboard lacks: Esc, Tab, arrows, Home, End, Page Up and Down, a few symbols, and Ctrl and Alt, which apply to the next key; a hardware keyboard works too. Drag to scroll back, pinch to change the text size, and use the menu to paste, copy the screen, switch between terminals, or open another. A terminal fits the screen as the keyboard comes and goes or the phone turns, and it keeps running when the link drops: the app takes it back when it reconnects, unless the agent closed it after `terminal.detached_timeout` or another client took it. The menu also lists the terminals open on other clients, such as the desktop app; choose one and press **Take over** to move it here, with its recent output.

**Ports** in a session's menu lists the TCP ports that programs of the agent's user listen on, on the agent's host, such as a development server. Press **Forward** on one, or type a port (or `host:port`), to forward a port of the phone to it: the forward listens on the phone's loopback address, on the same port number when it is free, so the phone's browser and other apps open the server at `localhost`. Forwards go on while Loom is not on screen, with a notification that lists them and stops them, and the app starts them again on the same ports when it reconnects or follows a restarted agent.

The app keeps its links to the agents while it runs and reconnects by itself, catching open conversations up on what they missed. When it comes back on screen or the phone changes networks, it checks each link at once instead of waiting for the link's heartbeat to time out. A restarted agent runs as a new instance; the app finds it by its workspace on the host's relay, and open screens follow it. When the app and an agent speak different protocol versions, the app says which of the two to update.

Built with a Firebase project, the app shows a notification when a tool call waits for approval and when a turn finishes or fails, also while it is closed; tapping one opens the session, and none comes for the session on screen. Each kind has its own channel in the system's notification settings. The app tells each paired host where to send them when it connects; the host's agents seal every notification for the phone with the keys from pairing and hand it to their relay, which sends it through Firebase Cloud Messaging, so Google sees only ciphertext. The relay needs a key of the same Firebase project (see [Through a relay](#through-a-relay)).

## Getting started

1. Create the agent configuration:

   ```sh
   loom-agent config init     # writes ~/.config/loom/agent.yaml
   ```

2. Start the client with `loom`. It lists the hosts from `~/.config/loom/client.yaml`. The default is just your own machine (`local`).

3. Start an agent for a project directory: click **+** next to a host under **Remotes** in the Sessions panel and type the directory, or run the agent yourself:

   ```sh
   loom-agent serve --workspace ~/src/my-project
   ```

   The client finds running agents on its own. For the local host it starts `loom-agent` from the same directory as `loom`, or from `PATH`. Set `LOOM_AGENT_BIN` to use a different binary.

4. Sign in to a model provider from the agent's menu (**Providers and sign-in…**), then start a session with `ctrl-n` or **+** next to the host.

### Remote hosts over SSH

Use **Add an SSH host** in the Sessions panel, or add an entry to `client.yaml`:

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
- `tools`: output limits for the `read` and `shell` tools, the shell program, the `search_web` tool (its model, source limits, and timeout), `notes.max_bytes`, the largest [notes](#notes-and-tasks) (64 KiB by default), and `tasks.max_items`, the most tasks in a task list (100 by default). `search_web` has an OpenAI model search the web. It uses the `openai-codex` sign-in and is not offered to the model without it. `allowed_dirs` lists directories outside the workspace that the `read`, `edit`, and `write` tools can read and change, for example `~/notes`. Each path must be absolute or start with `~`. The system prompt lists these directories. The policy still applies to each tool call, and the sandbox of shell commands does not change.
- `tools.comment_guard`: stops the model from adding comments to code. See [Comment guard](#comment-guard).
- `policy`: whether each tool call is allowed, denied, or needs your approval (`ask`, the default). Rules can match a tool and a command prefix, for example to allow `git status` but deny `sudo`. The notes, tasks, and `skill` tools are allowed unless a rule says otherwise. `allow` still asks for tool calls that may be harmful, and `allow_harmful` allows them too. See [Harmful tool calls](#harmful-tool-calls).
- `environment`: the environment variables of shell commands and terminals, and the env file with secrets for them. See [Environment variables and secrets](#environment-variables-and-secrets).
- `worktrees`: `dir`, the directory of the git worktrees of sessions (`~/.config/loom/worktrees` by default). See [Sessions in a worktree](#sessions-in-a-worktree).
- `sandbox`: confines shell commands with `landlock` or `bubblewrap` (off by default), with optional network blocking and extra writable paths. Changes in the writable paths are not [harmful](#harmful-tool-calls), also when the sandbox is off.
- `context`: tool output limits and when to compact a long conversation.
- `limits`: `max_rounds_per_turn`, the most model calls the agent makes for one message before the turn stops (200 by default; `0` for no limit). It guards against a model stuck in a loop while no one is watching. The conversation is kept, so you can send another message to go on.
- `instructions`: extra instructions for the model. `AGENTS.md` files in the workspace are included too.
- `skills`: `enabled` (on by default), `paths`, more directories of [skills](#skills), and `disabled`, the names of skills not to offer.
- `relay`: relay connection settings.
- `telemetry`: OpenTelemetry (OTLP) export of metrics and events, off by default. See [Telemetry](#telemetry).

`crates/loom-config/tests/fixtures/agent.yaml` is an example that uses most of these settings.

### Environment variables and secrets

Shell commands of the agent and terminals get environment variables in this order:

1. The agent's own environment.
2. `inherit`: `all` (the default) keeps all of it, `core` keeps only `HOME`, `LOGNAME`, `PATH`, `SHELL`, `USER`, `USERNAME`, the temporary directories, the locale, and `TZ`, and `none` keeps nothing.
3. The default excludes: variables whose names contain `KEY`, `SECRET`, `TOKEN`, or `PASSWORD` are removed. Set `ignore_default_excludes: true` to keep them.
4. `exclude`: name patterns of more variables to remove.
5. The env file, `env_file` (`.env` in the workspace by default; `null` for none).
6. `set`: variables with fixed values.
7. `include_only`: if it is not empty, only the variables that match it stay.

Patterns use `*` for any text and `?` for one character, and ignore case.

Put the credentials of CLIs, such as `aws` or `lakectl`, in the env file of the workspace:

```sh
# .env
LAKECTL_CREDENTIALS_ACCESS_KEY_ID=AKIA...
LAKECTL_CREDENTIALS_SECRET_ACCESS_KEY=...
AWS_REGION=eu-west-1
```

```yaml
environment:
  redaction:
    allow: [AWS_REGION]
```

The agent reads the env file again when it changes. If the file cannot be parsed, commands keep the values from before, and the agent shows a warning with the line number.

The model does not get the values of the variables from the env file and `set`, except those in `redaction.allow`. The system prompt lists only the names of the variables. In tool output, the values, and their base64 and URL-encoded forms, show as `[REDACTED:<NAME>]`. This also applies to the values of the agent's own variables with secret names, also when commands do not get them. The agent hides the values before it records tool output, saves it to a file, or sends it to clients and the model. The `read` tool shows the env file with its values hidden, and the `write` and `edit` tools cannot change it: only you change it. With the `bubblewrap` sandbox, commands cannot read the env file. Values shorter than 8 characters cannot be hidden; the agent warns about them. The agent also warns when git does not ignore the env file. The warnings show above the system prompt (**System prompt** in a session's menu). Terminals get the same variables, and their output is not hidden, because only you see it. The files that you open in a client also show the raw env file. Set `redaction.enabled: false` to turn off the hiding and the protection of the env file.

Redaction is best effort. A command can still change a value in ways that the agent does not detect, for example when it prints the value backwards, and it can send a value over the network. Thus, give the agent credentials with the least privileges, and set `sandbox.network: false` when the commands do not need the network.

When systemd runs the agent, `~/.config/loom/agent.env` sets variables of the agent's own environment. Commands get these variables too, unless the rules above remove them.

### Telemetry

The `telemetry` section makes the agent export metrics and events over OTLP. The data is modelled on the telemetry of Claude Code. `loom-agent config fetch-telemetry` prints this section from the settings that your organization publishes for Claude Code. It uses the Anthropic sign-in of the agent host.

To give the data the names of Claude Code's data, set these values:

```yaml
telemetry:
  prefix: claude_code        # metric names such as claude_code.token.usage
  service_name: claude-code  # the service.name resource attribute
```

Each metric and event tells who uses the agent, with the attributes of Claude Code:

- `user.email`: `telemetry.user_email`, else the email of the Anthropic sign-in, else `git config --global user.email`.
- `user.account_uuid`: `telemetry.account_uuid`, else the account of the Anthropic sign-in. `user.account_id` is the same account in the form of Claude Code, such as `user_01…`. Set `include_account_uuid: false` to leave out both. `fetch-telemetry` copies this choice from `OTEL_METRICS_INCLUDE_ACCOUNT_UUID`.
- `organization.id`: `telemetry.organization_id`, else the organization of the Anthropic sign-in. A sign-in from Loom 0.4.0 or earlier gets its organization at its next token refresh.

The agent reads these values when it starts. After you sign in, or after you change the `telemetry` section, restart the agent.

### Comment guard

The comment guard stops the model from adding comments to code. The `edit` and `write` tools compare each change with the file before the change, and find the comments that the change adds. The guard allows these comments:

- Changes to existing comments, for example new words in a comment.
- New comment lines next to an existing comment, for example one more line in a doc comment.
- Comments that the change moves, also from one file to a different file in one patch.
- Tool directives, such as `eslint-disable`, `@ts-expect-error`, `# noqa`, `# type: ignore`, and `#!/usr/bin/env`.

The guard knows the comments of common languages by the file extension, such as `.rs`, `.ts`, `.py`, `.go`, `.sh`, `.yaml`, `.sql`, and `.html`. It does not check other files.

```yaml
tools:
  comment_guard:
    mode: block              # block (default), warn, or off
    allow_directives: true   # allow tool directives
    allow_patterns: ["TODO\\(#\\d+\\)"]   # regular expressions of comments to allow
    ignore_paths: [vendor, docs/examples] # paths in the workspace not to check
```

In `block` mode, the tool rejects a change with new comments and tells the model which comments to remove. In `warn` mode, the tool makes the change and tells the model about the new comments. While the guard is on, the system prompt tells the model not to add comments.

You can turn the guard on or off for one session. In the desktop app, use **Comment guard** in the status bar. In the Android app, use the comment guard item in the row above the prompt box. **Default** uses the `mode` of `agent.yaml`. **On** uses `block` when `mode` is `off`.

### Harmful tool calls

When the policy allows a `shell`, `edit`, or `write` call, the agent first checks if the call may be harmful. If it may be harmful, the agent asks for your approval, and shows the reason. These calls may be harmful:

- `rm`, `rmdir`, `unlink`, `shred`, `mv`, `cp`, `chmod`, `chown`, `chgrp`, `tee`, `touch`, `truncate`, `dd`, and `ln` on a path outside the workspace.
- Output redirects (`>` and `>>`) to a path outside the workspace.
- `find -delete`, `find -exec rm`, `xargs rm`, and `rsync --delete` on a path outside the workspace.
- Changes to the `.env` files (but not `.env.example`) and to the `.git` directory at the root of the workspace.
- Destructive git commands: `git checkout --`, `git restore` (but not `git restore --staged`), `git reset --hard`, `git reset --merge`, `git push --force`, `git clean -f`, `git branch -D`, `git stash drop`, and `git stash clear`.
- `edit` and `write` calls on a protected path or a denied path.

Changes in the temporary directories (`/tmp`, `/var/tmp`, and `$TMPDIR`) and in `sandbox.writable` are not harmful. Changes by the `edit` and `write` tools in `tools.allowed_dirs` are not harmful too, but changes by shell commands there are. Changes to `/dev/null` and the standard streams are not harmful, but their deletion is.

The check examines each command in `&&`, `||`, `;`, `|`, and newline chains. It follows `cd`, variables set earlier in the command, and the environment of commands. It resolves symbolic links, also of files that do not exist yet. It does not examine heredoc bodies, quoted text, and shell comments. If it cannot find the path of a target, such as `$(...)`, the call may be harmful.

The policy actions and the check:

- `ask`: the agent asks for each call. For a call that may be harmful, the approval request shows the reason.
- `allow`: the agent runs the call without asking, unless the call may be harmful.
- `allow_harmful`: the agent runs the call without the check.
- `deny`: the agent refuses the call.

Rules can use `allow_harmful` too, for example to allow `git push --force` in a workspace. You can also pick **Allow harmful** for one session in the desktop app and in the Android app. **Allow for session** on the approval of a call that may be harmful adds an `allow` rule, so the agent asks again for the next call that may be harmful.

```yaml
policy:
  default: allow
  harmful:
    deny_paths: [./infra/production]  # paths whose changes are always harmful
sandbox:
  writable: [~/.cargo, ~/scratch]    # paths whose changes are not harmful
```

The paths in `sandbox.writable` can be absolute paths or paths that start with `~`. A writable path also allows the `.env` and `.git` paths and the deletion of `/dev/null` in it.

The paths in `deny_paths` can be absolute paths, paths that start with `~`, or paths relative to the workspace. `deny_paths` has priority over `sandbox.writable` and over the workspace. Changes to the parent directories of a denied path are harmful too. This also applies to glob patterns that can include a denied path.

The check is not a sandbox. It cannot find all harmful commands, for example commands in a script file. To confine commands, use `sandbox` too.

### Reloading the configuration

A running agent applies `agent.yaml` again when the file changes. To make it read the file on demand, for example when it missed a change or you want to know whether it accepted the file, run `loom-agent reload --workspace <dir> | --instance <id> | --all`, or choose **Reload configuration** in an agent's menu under **Remotes** in the client's Sessions panel. Each agent says whether it reloaded the file, found it unchanged, or rejected it; `loom-agent reload` exits with status 1 if any agent rejected the file or could not be reached.

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
| `loom-agent update [--check] [--version <version>]` | Update `loom-agent` from the latest release, or another one; see [Updating](#updating). |

## Client configuration

The client reads `~/.config/loom/client.yaml`. Besides `hosts`, it has:

- `appearance`: theme, UI and monospace fonts and sizes, fallback fonts for glyphs the monospace font lacks (for example Hangul in terminals), and `wrap_lines`, whether file and diff views open with long lines wrapped (off by default).
- `keymap`: overrides for the default key bindings.
- `check_for_updates`: whether the desktop client looks for a newer release when it starts and once a day after that (on by default); see [Updating](#updating).

Useful default key bindings:

| Keys | Action |
| --- | --- |
| `ctrl-shift-p` | Command palette |
| `ctrl-n` | New session |
| `ctrl-p` | Find a session |
| `ctrl-alt-o` | Find an agent |
| `ctrl-shift-m` | Choose the model |
| `` ctrl-` `` | New terminal |
| `ctrl-b` | Show or hide the Sessions panel |
| `ctrl-shift-b` / `ctrl-shift-n` / `ctrl-shift-t` | Sessions panel / the agent's notes / the session's tasks |
| `enter` / `ctrl-enter` while a turn runs | Queue the message for the next turn / steer the running turn with it |
| `ctrl-shift-e` / `ctrl-shift-g` | Workspace files / git changes |
| `ctrl-tab` / `ctrl-shift-tab` (or `ctrl-pagedown` / `ctrl-pageup`) | Next / previous tab in the pane |
| `alt-1` … `alt-8` / `alt-9` | Tab 1 to 8 / the last tab in the pane |
| `ctrl-shift-pagedown` / `ctrl-shift-pageup` | Move the tab right / left |
| `ctrl-w` | Close the tab, unless it is the session's conversation or another client's terminal |
| `ctrl-k` then an arrow | Split the pane |
| `ctrl-k ctrl-t` | Select a theme |
| `ctrl-s` | Save the file being edited in a file view, or the notes in the Notes tab |
| `alt-z` | Wrap long lines in a file or diff view, or stop wrapping them |

A session has one of these states:

| State | What the session does |
| --- | --- |
| Thinking | The model streams its reasoning, or it has not sent output yet. |
| Responding | The model streams its answer: text or tool calls. |
| Running a tool | The agent runs tool calls. This includes the time a call waits for another session to release a file or the workspace. |
| Waiting for approval | A tool call waits for your approval. |
| Completed | The turn completed, but you did not open the session since. |
| Idle | You opened the finished session. A turn that failed or that you stopped also makes the session idle. |

The Sessions panel groups the sessions of the connected agents by state: **Needs approval** (waiting for approval), **Working** (thinking, responding, or running a tool), **Completed**, **Idle**, and **Archived**. Under **Remotes**, the panel shows each host with its agents: the workspace of each agent, its number of sessions, and whether the client is connected to it. Click an agent that is not connected to connect to it. The menu of an agent has the actions for the agent, such as **Stop agent**.

To start a session, click **+** next to a host. The **New session** dialog lists the workspaces of the agents on the host. Click a workspace to start a session there, or click **In a worktree…** to start a session in a new worktree. To start a session in another directory, type the directory and click **Start**. The host then starts an agent for the directory, and the session opens when the client is connected to the agent. A completed session becomes idle when you show it in a client while the client's window is active, and the agent tells all clients.

A session's conversation tab stays while the session is open. To close a session with all its tabs, use **session: close** in the command palette; the session stays on the agent, and you can open it again.

A terminal is open on one client at a time, and every client shows the agent's terminals. A terminal that another client opened, such as the Android app, gets a tab in the agent's active session, titled with where it is open. The tab shows the terminal dimmed; press **Take over** on it to move the terminal here with its recent output (up to 128 KiB); the other client can take it back the same way. Such a tab cannot be closed until you take the terminal over, since closing a terminal's tab ends its shell; closing its session moves the tab to another open session of the agent.

The tab and panel keys work in terminals too, so a terminal program does not see them (for example `alt-1` in readline). `ctrl-w`, `ctrl-b`, and `ctrl-p` go to the terminal.

Images (PNG, JPEG, WebP, GIF, BMP, TIFF, and icons) and PDF files open in a viewer instead of the text editor, up to 64 MB. Its toolbar zooms in and out, fits the image to the view or the pages to its width, and shows the image at its own size; a PDF file's pages are drawn as they scroll into view. SVG files stay text.

The **Ports** tab of the right panel (also **panel: ports** in the command palette) lists the TCP ports that programs of the agent's user listen on, on the agent's host. **Forward** on a port, or a port (or `host:port`) typed into its field, forwards a local port of this machine to it: the same port number when it is free here, else another one. Each forward shows its local address, with buttons to open it in the browser, copy it, and stop it. Forwards work over every kind of connection, also through a relay, and go on after the client reconnects to the agent. The agent finds listening ports on Linux only; elsewhere, type the port.

To copy a workspace file to your machine, right-click it in the files panel and choose **Download…**, or use the download button in its file view. Downloads work over every kind of connection, also through a relay.

To copy files of your machine to the workspace, drag them onto a folder in the files panel, right-click a folder and choose **Upload files…**, or use the upload button above the panel, which uploads to the selected folder. Files dropped on a file go into its folder. When a file of the same name is in the folder, the client asks before it replaces it. A file appears in the workspace only once all of it arrived; a cancelled or failed upload leaves nothing behind. Uploads work over every kind of connection, also through a relay. The Android app cannot upload files.

When you open a session, the client loads its latest 50 messages and tool calls, and loads earlier ones as you scroll to the top. The **Changes** panel loads the whole session, to list every file it changed.

The **Changes** panel of the desktop app lists the uncommitted changes in git. It also lists the commits that the base branch does not have yet, and the files that these commits change. For a session in a worktree, the base is the base branch of the worktree. For other sessions, the base is the upstream branch of the checked-out branch, for example `origin/main`; without an upstream branch, the panel shows no commits. Click a file under **Committed since** to open its diff from the base to `HEAD`, in a tab of its own. The Android app shows only the uncommitted changes.

Each code block in an answer has a copy button, which copies the code without the fences. Each table has a copy button below it, which copies the table as Markdown. The Android app has the same buttons.

To start a session's conversation over, press **Clear** under the prompt box. The agent forgets the conversation and its history for good; the session keeps its name, model, reasoning effort, its tasks, and the tool approvals you gave it, and the agent's notes stay.

To see what the model gets besides the conversation, choose **System prompt** in a session's menu in the Sessions panel, or **session: show system prompt** in the command palette. A tab shows the system prompt that the session's next model call sends: the built-in instructions (or `instructions.system_prompt`), the `AGENTS.md` files, and the environment. Problems with the instructions, such as an `AGENTS.md` file that was too large, show above it. **Tools** lists the tools that the model can call; click a tool to see its description and the JSON schema of its parameters. The refresh button reads the prompt again, for example after you change an `AGENTS.md` file. On Android, choose **System prompt** in the menu of a session.

### Rewinding a conversation

To take a conversation back to before one of your messages, click the rewind button on that message, or long-press the message in the Android app and choose **Rewind to here**. The agent forgets that message and everything after it, and the message goes back into the prompt box, so you can edit it and send it again. Every client of the agent shows the shorter conversation.

A rewind changes only the conversation. Files in the workspace stay as they are, so the agent does not undo the changes of the forgotten turns. Tasks, the agent's notes, the session's settings, and waiting messages also stay. You cannot rewind while a turn runs. After a compaction summarized the earlier turns, you can rewind only to the last message before the summary and to later ones. A rewind cannot be undone.

### Sessions in a worktree

A session can work in a git worktree of its own, so that its changes stay away from the workspace and from other sessions until you merge them. On the desktop, click **+** next to the host and then **In a worktree…** next to the workspace, or choose **New session in a worktree…** in an agent's menu under **Remotes**. In the Android app, press the branch button next to the agent when you start a session. Type a branch name and a base, or keep the defaults: the branch `loom/<session>`, made from the branch that the workspace has checked out. The workspace must be in a git repository.

The agent adds the worktree in `<worktrees.dir>/<workspace-hash>/<session>`, in directories with mode 0700. The agent's tools, shell commands, `AGENTS.md`, and skills of the session then use the worktree, and the system prompt tells the model the branch. The Files and Changes panels and new terminals follow the active session's worktree. The Changes panel also shows the commits of the branch that are not in its base yet. The Sessions panel and the Android app show the branch next to the session. The agent's notes are the same for all sessions, also in worktrees. Shell commands in a worktree get the env file of the workspace (see [Environment variables and secrets](#environment-variables-and-secrets)), with the same redaction. With the sandbox on, commands can also write the repository's git directory, so that they can commit.

When the work is done, use the session's menu (in the Sessions panel, or the conversation's menu on Android):

- **Merge worktree** merges the branch into its base in the workspace. The workspace must have the base checked out and no uncommitted changes to tracked files. With a commit message, the agent squashes the branch into one commit; without one, it makes a merge commit that keeps the commits of the branch. If the merge has conflicts, the workspace stays as it was and the error lists the files: resolve them in the worktree, for example by merging the base into the branch there, then merge again.
- **Open pull request** pushes the branch to the remote of the base (`origin` if the base has none) and runs `gh pr create` with your title, description, and draft choice. The [GitHub CLI](https://cli.github.com/) (`gh`) must be installed and signed in on the agent's host. The session keeps the URL of the pull request; after that, the menu pushes new commits to it and opens it in the browser.
- **Remove worktree** removes the worktree, its branch, and the session. A session does not continue without its worktree. If the worktree has uncommitted changes or commits that were not merged, the dialog tells you that you lose them.

Deleting a session also removes its worktree and branch. Archiving a session keeps them. A merge and a pull request keep the worktree and the session, so you can go on working in it. If the worktree directory is gone, the session does not start turns; remove the worktree to delete the session.

Worktrees are full checkouts of your repository. If a dotfile manager or a backup tool syncs `~/.config`, exclude `~/.config/loom/worktrees`, or set `worktrees.dir` to another directory.

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

### Skills

Skills give the agent instructions for special tasks, in the [Agent Skills](https://agentskills.io) format. A skill is a directory with a `SKILL.md` file. The file starts with YAML frontmatter that has the skill's `name` and `description`, and then has the instructions in Markdown. The directory can also hold other files, such as references and scripts.

The agent looks for skills in these directories, in this order:

1. `.loom/skills/` and `.agents/skills/` in the workspace
2. `~/.config/loom/skills/` and `~/.agents/skills/`
3. The directories in `skills.paths` in `agent.yaml`. A relative path is relative to the workspace. A directory can hold skills or be one skill.

When two skills have the same name, the first one found wins, so a skill of the workspace replaces a skill of the user. The agent reads the directories before each model call, so new and changed skills apply at once.

The system prompt lists the name and the description of each skill. When a task matches a skill, the model calls the `skill` tool with the skill's name. The tool returns the instructions without the frontmatter, the skill's directory, and a list of its other files. The list does not include hidden files and files that tools make: `__pycache__`, `node_modules`, Python virtual environments, `*.pyc`, `*.pyo`, `*~`, and `Thumbs.db`. The `read` tool can read the files in the directory of each listed skill, also outside the workspace; the other file tools cannot change them. The model runs the scripts of a skill with the `shell` tool, which follows the normal policy. When the conversation is compacted, the instructions of the loaded skills stay in the context.

A skill without a description, or without frontmatter, is skipped, and the agent shows why as a warning. The agent ignores the other fields of the frontmatter, such as `allowed-tools`. Set `skills.enabled: false` to turn off skills, or list the names of single skills in `skills.disabled`. To see the skills that a session offers, open its system prompt (see [Client configuration](#client-configuration)).

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

## License

Loom is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE). Some code is adapted from [Zed](https://github.com/zed-industries/zed), and some themes come from [gpui-component](https://github.com/longbridge/gpui-component). Gotham is based on [vim-gotham](https://github.com/whatyouhide/vim-gotham), and the other themes are generated from the palettes at [TerminalColors](https://terminalcolors.com/). [NOTICE](NOTICE) lists the details and the authors of the color schemes.
