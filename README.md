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

The [releases page](https://github.com/chiwanpark/loom/releases) has prebuilt archives for x86-64 Linux:

- `loom-<version>-x86_64-unknown-linux-gnu.tar.gz` holds the desktop client `loom` together with `loom-agent`. It needs glibc 2.39 or newer (Ubuntu 24.04, Debian 13), `libxkbcommon`, a Wayland session, and a Vulkan driver.
- `loom-agent-<version>-x86_64-unknown-linux-musl.tar.gz` and `loom-relay-<version>-x86_64-unknown-linux-musl.tar.gz` are statically linked and run on any x86-64 Linux, such as the remote hosts you reach over SSH.

Unpack an archive and put its binaries on your `PATH`, for example in `~/.local/bin`. `SHA256SUMS` on the release page lists the archive checksums. Every binary prints its version and the commit it was built from with `--version`. [RELEASING.md](RELEASING.md) describes how releases are made.

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

Install `loom-agent` on every machine where you want an agent to run, including remote hosts you reach over SSH.

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

1. Run `loom-relay serve` somewhere both sides can reach (`loom-relay config init` writes a starting configuration).
2. In the agent's `agent.yaml`, set `relay.enabled: true` and `relay.url`.
3. On the agent's host, run `loom-agent pair`. It prints a one-time `loom://` link (and a QR code) that expires after 10 minutes by default.
4. In the client, use **Pair through a relay** and paste the link.

`loom-agent clients list` shows the paired clients, and `loom-agent clients revoke <label>` removes one.

To keep an agent running for the relay, `contrib/systemd/loom-agent.service` starts `loom-agent serve --default` (the `lifecycle.default_workspace` directory) as a systemd user service. Installation steps are in the file.

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
- `tools`: output limits for the `read` and `shell` tools, the shell program, and the `search_web` tool (its model, source limits, and timeout). `search_web` has an OpenAI model search the web. It uses the `openai-codex` sign-in and is not offered to the model without it.
- `policy`: whether each tool call is allowed, denied, or needs your approval (`ask`, the default). Rules can match a tool and a command prefix, for example to allow `git status` but deny `sudo`.
- `sandbox`: confines shell commands with `landlock` or `bubblewrap` (off by default), with optional network blocking and extra writable paths.
- `context`: tool output limits and when to compact a long conversation.
- `instructions`: extra instructions for the model. `AGENTS.md` files in the workspace are included too.
- `relay`: relay connection settings.
- `telemetry`: OpenTelemetry (OTLP) export of metrics and events, off by default. `loom-agent config fetch-telemetry` prints this section from the settings your organization publishes for Claude Code.

`crates/loom-config/tests/fixtures/agent.yaml` is an example that uses most of these settings.

## Agent commands

| Command | What it does |
| --- | --- |
| `loom-agent serve --workspace <dir>` | Run an agent for a workspace. `--default` serves `lifecycle.default_workspace`. |
| `loom-agent ps [--json]` | List the agents running on this host. |
| `loom-agent stop --workspace <dir> \| --instance <id> \| --all` | Stop agents gracefully. |
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
| `ctrl-shift-e` / `ctrl-shift-g` | Workspace files / uncommitted changes |
| `ctrl-k` then an arrow | Split the pane |
| `ctrl-k ctrl-t` | Select a theme |
| `ctrl-s` | Save the file being edited in a file view |
| `alt-z` | Wrap long lines in a file or diff view, or stop wrapping them |

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
cargo deny check
```

## License

Loom is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE). Some code is adapted from [Zed](https://github.com/zed-industries/zed), and the themes come from [gpui-component](https://github.com/longbridge/gpui-component). [NOTICE](NOTICE) lists the details.
