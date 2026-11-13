<p><img src="assets/logo.svg" alt="Loom logo" width="128" height="128"></p>

# Loom

Loom is a coding agent split into two parts. An **agent** runs on the machine that holds the code and does the work there: it talks to the model, reads and edits files, and runs shell commands. A **client** (the `loom` desktop app) connects to any number of agents on your own machine, over SSH, or through a relay, and shows their sessions, terminals, file changes, and diffs.

Agents keep running when the client disconnects, so you can close the app, reopen it later or on another machine, and pick up the same sessions.

Loom is early software. Configuration formats and the protocol may still change between 0.x versions; [CHANGELOG.md](CHANGELOG.md) lists what changed in each release.

## Components

| Binary | Crate | Purpose |
| --- | --- | --- |
| `loom-agent` | `crates/loom-agent` | The agent daemon and its command-line tools. It serves one workspace directory. |
| `loom` | `crates/loom-gui` | The desktop client, built with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui). Linux and Wayland only. |
| `loom-relay` | `crates/loom-relay` | An optional WebSocket relay so clients can reach agents behind NAT without SSH. |

The other crates are libraries: the agent core (`loom-agent-core`), model providers (`loom-llm`), tools and sandboxing (`loom-tools`), the wire protocol (`loom-proto`), transports (`loom-transport`), the client library (`loom-client`), configuration (`loom-config`), key handling (`loom-crypto`), terminal emulation (`loom-vt`), and OpenTelemetry export (`loom-telemetry`).

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
4. In the client, use **Pair through a relay** and paste the link.

`loom-agent clients list` shows the paired clients, and `loom-agent clients revoke <label>` removes one.

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
| `ctrl-shift-b` / `ctrl-shift-n` / `ctrl-shift-t` | Agents panel / the session's notes / the session's tasks |
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

To start a session's conversation over, press **Clear** under the prompt box. The agent forgets the conversation and its history for good; the session keeps its name, model, reasoning effort, its notes and tasks, and the tool approvals you gave it.

### Notes and tasks

Each session has notes and a task list for the agent's work. Both stay when the conversation is compacted or cleared, and are deleted with the session. They are not files in the workspace, and their contents are not added to the model's instructions: the agent reads them when it needs to, and its instructions tell it to read them again after a long conversation is compacted.

- The **notes** are a text scratchpad for findings, decisions, and open questions. The agent reads and changes them with the `notes_read`, `notes_edit`, and `notes_write` tools.
- The **task list** is the plan of the work: numbered tasks, each created, in progress, or done. The agent reads it with `tasks_read`, and adds, changes, and removes tasks with `tasks_update`, which applies a batch of changes all or none.

The **Notes** tab of the right panel (`ctrl-shift-n`, or **panel: notes** in the command palette) shows the active session's notes, and you can edit them there too: **Save** (`ctrl-s`) stores your text, **Revert** throws your changes away, and **Clear notes** empties them. When the agent changes the notes while you have unsaved changes, yours stay and the tab offers to reload the agent's text or overwrite it with yours. So that your edits are not lost unseen, `notes_write` refuses to replace non-empty notes the agent has not read since you last changed them or since its conversation was compacted or cleared.

The **Tasks** tab (`ctrl-shift-t`, or **panel: tasks**) shows the task list. Type a task in the box at the bottom and press `enter` to add it (`shift-enter` starts a new line). Click a task's circle to move it on from created to in progress to done, click its text to edit it (`enter` saves, `escape` cancels), and click the trash icon to remove it; **Clear done** removes the tasks that are done. Your changes apply at once, and when you and the agent change the list at the same time, the later change wins.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

## License

Loom is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE). Some code is adapted from [Zed](https://github.com/zed-industries/zed), and the themes come from [gpui-component](https://github.com/longbridge/gpui-component), except Gotham, which is based on [vim-gotham](https://github.com/whatyouhide/vim-gotham). [NOTICE](NOTICE) lists the details.
